//! The OpenAI dialect and strict schemas (T3.3, TU-26): render, translate,
//! the recorded vendor items, and the strict null rule through the registry.
use agentkit_core::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/dialect-openai-v1.json")).expect("fixture")
}

fn definitions(values: &Value) -> Vec<ToolDefinition> {
    values
        .as_array()
        .expect("array")
        .iter()
        .map(|value| serde_json::from_value(value.clone()).expect("definition"))
        .collect()
}

fn names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

fn canonical() -> Vec<ToolDefinition> {
    definitions(&fixture()["canonical"])
}

// ---- render ---------------------------------------------------------------

#[test]
fn write_edit_and_run_become_the_patch_and_shell_tools() {
    let rendered = openai_dialect().render(&canonical());
    assert_eq!(
        names(&rendered),
        fixture()["request"]["toolNames"]
            .as_array()
            .unwrap()
            .clone()
    );
    assert_eq!(builtin_tool_type(&rendered[0]), Some("apply_patch"));
    assert_eq!(builtin_tool_type(&rendered[1]), Some("shell"));
    assert_eq!(builtin_tool_type(&rendered[2]), None);
    // The patch tool can write whichever canonical tool it reaches.
    assert_eq!(rendered[0].effect, "write");
    assert_eq!(rendered[0].capability, "fs.write");
    assert_eq!(rendered[1].capability, "process.exec");
    assert!(rendered[0].description.is_empty() && rendered[1].description.is_empty());
    assert!(openai_dialect().collisions(&canonical()).is_empty());
}

#[test]
fn a_built_in_is_offered_only_for_tools_the_session_has() {
    let all = canonical();
    let only = |keep: &[&str]| -> Vec<ToolDefinition> {
        all.iter()
            .filter(|d| keep.contains(&d.name.as_str()))
            .cloned()
            .collect()
    };
    let dialect = openai_dialect();
    assert_eq!(names(&dialect.render(&only(&["run_command"]))), ["shell"]);
    assert_eq!(
        names(&dialect.render(&only(&["edit_file"]))),
        ["apply_patch"]
    );
    assert_eq!(
        names(&dialect.render(&only(&["calendar_list"]))),
        ["calendar_list"]
    );
    assert!(dialect.render(&[]).is_empty());
}

#[test]
fn other_tools_get_strict_schemas_and_examples_that_fit_them() {
    let rendered = openai_dialect().render(&canonical());
    let expected = &fixture()["request"]["calendarList"];
    assert_eq!(serde_json::to_value(&rendered[2]).unwrap(), *expected);
    assert!(is_strict_compliant(&rendered[2].input_schema));
    // The canonical schema is not strict: `limit` is optional.
    assert!(!is_strict_compliant(&canonical()[3].input_schema));
    // Every example validates against the strict schema it is shown with.
    let validator = agentkit_contracts::compile_tool_schema(&rendered[2].input_schema).unwrap();
    for example in &rendered[2].examples {
        assert!(validator.iter_errors(example).next().is_none(), "{example}");
    }
}

/// `required` lists in a fixed order, so the check does not depend on how
/// the JSON map orders its keys.
fn sorted(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "required"
                    && let Value::Array(names) = child
                {
                    names.sort_by_key(|name| name.to_string());
                } else {
                    *child = sorted(child.take());
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| *item = sorted(item.take())),
        _ => {}
    }
    value
}

#[test]
fn strict_rewrite_marks_optional_nested_and_enum_properties_nullable() {
    let schema = json!({
        "type": "object",
        "properties": {
            "mode": {"type": "string", "enum": ["a", "b"]},
            "tags": {"type": "array", "items": {"type": "string"}},
            "opts": {
                "type": "object",
                "properties": {"deep": {"type": "boolean"}, "name": {"type": "string", "minLength": 1}},
                "required": ["name"],
                "additionalProperties": false
            }
        },
        "required": ["opts"],
        "additionalProperties": false
    });
    let strict = strict_schema(&schema).expect("strict");
    assert_eq!(
        sorted(strict.clone()),
        sorted(json!({
            "type": "object",
            "properties": {
                "mode": {"type": ["string", "null"], "enum": ["a", "b", null]},
                "tags": {"type": ["array", "null"], "items": {"type": "string"}},
                "opts": {
                    "type": "object",
                    "properties": {"deep": {"type": ["boolean", "null"]}, "name": {"type": "string"}},
                    "required": ["deep", "name"],
                    "additionalProperties": false
                }
            },
            "required": ["mode", "tags", "opts"],
            "additionalProperties": false
        }))
    );
    assert!(is_strict_compliant(&strict));
}

#[test]
fn a_schema_strict_mode_cannot_take_is_offered_as_it_is() {
    for schema in [
        json!({"type":"object","properties":{"a":{"oneOf":[{"type":"string"},{"type":"number"}]}}}),
        json!({"type":"object","properties":{"a":{"type":"object"}}}),
        json!({"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":true}),
        json!({"type":"object","properties":{"a":{"type":"string","contentEncoding":"base64"}}}),
    ] {
        assert!(strict_schema(&schema).is_none(), "{schema}");
        assert!(!is_strict_compliant(&schema), "{schema}");
    }
    let mut odd = canonical()[3].clone();
    odd.input_schema = json!({"type":"object","properties":{"a":{"oneOf":[{"type":"string"}]}}});
    let rendered = openai_dialect().render(&[odd.clone()]);
    assert_eq!(rendered[0].input_schema, odd.input_schema);
}

#[test]
fn a_tool_with_no_parameters_gets_a_closed_empty_schema() {
    let strict = strict_schema(&json!({"type":"object"})).expect("strict");
    assert_eq!(
        strict,
        json!({"type":"object","properties":{},"required":[],"additionalProperties":false})
    );
}

#[test]
fn chat_completions_keeps_the_common_shape_with_strict_schemas() {
    let dialect = openai_chat_dialect();
    assert_eq!(dialect.id, "openai-chat");
    let rendered = dialect.render(&canonical());
    assert_eq!(
        names(&rendered),
        ["write_file", "edit", "run_shell_command", "calendar_list"]
    );
    assert!(rendered.iter().all(|d| builtin_tool_type(d).is_none()));
    // write_file's schema is already strict; edit's optional replace_all becomes nullable.
    assert!(is_strict_compliant(&rendered[0].input_schema));
    assert!(is_strict_compliant(&rendered[1].input_schema));
    assert_eq!(
        rendered[1].input_schema["properties"]["replace_all"]["type"],
        json!(["boolean", "null"])
    );
    assert!(
        rendered[1].input_schema["properties"]["old_string"]
            .get("minLength")
            .is_none()
    );
    assert_eq!(
        rendered[3].input_schema,
        fixture()["request"]["calendarList"]["inputSchema"]
    );
}

#[test]
fn the_dialect_is_known_by_id_and_version() {
    assert_eq!(
        lookup_dialect("openai", "1").map(|d| d.id),
        Some("openai".to_owned())
    );
    assert_eq!(
        lookup_dialect("openai-chat", "1").map(|d| d.id),
        Some("openai-chat".to_owned())
    );
    assert!(lookup_dialect("openai", "2").is_none());
    assert!(openai_dialect().strict() && openai_chat_dialect().strict());
    assert!(!common_dialect().strict());
    assert!(strict_dialect("openai") && !strict_dialect("anthropic"));
}

// ---- translate ------------------------------------------------------------

#[test]
fn recorded_vendor_calls_translate_to_the_canonical_calls() {
    let dialect = openai_dialect();
    for pair in fixture()["response"]["calls"].as_array().expect("calls") {
        let wire = &pair["wire"];
        let translated = dialect
            .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
            .unwrap_or_else(|error| panic!("{wire}: {error}"));
        assert_eq!(translated.name, pair["canonical"]["name"], "{wire}");
        assert_eq!(translated.args, pair["canonical"]["arguments"], "{wire}");
        assert!(translated.args_changed, "{wire}");
        let back = dialect.wire_call(
            pair["canonical"]["name"].as_str().unwrap(),
            &pair["canonical"]["arguments"],
        );
        if pair["roundTrips"] == true {
            assert_eq!(
                back,
                Some((
                    wire["name"].as_str().unwrap().to_owned(),
                    wire["arguments"].clone()
                ))
            );
        }
    }
}

#[test]
fn recorded_vendor_items_carry_the_payload_the_dialect_reads() {
    let dialect = openai_dialect();
    let items = &fixture()["response"]["items"];
    let patch = dialect
        .translate("apply_patch", items[0]["operation"].clone())
        .expect("patch");
    assert_eq!(patch.name, "edit_file");
    assert_eq!(patch.args["old_string"], "def fib(n):");
    let shell = dialect
        .translate("shell", items[1]["action"].clone())
        .expect("shell");
    assert_eq!(
        (shell.name.as_str(), &shell.args["command"]),
        ("run_command", &json!("ls -l"))
    );
    assert_eq!(shell.args["timeout"], 120000);
}

#[test]
fn calls_the_canonical_tools_cannot_express_are_refused_with_a_reason() {
    let dialect = openai_dialect();
    for case in fixture()["response"]["refused"]
        .as_array()
        .expect("refused")
    {
        let wire = &case["wire"];
        let error = dialect
            .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
            .expect_err(&wire.to_string());
        assert!(
            error.message.contains(case["mentions"].as_str().unwrap()),
            "{wire}: {}",
            error.message
        );
    }
}

#[test]
fn bad_vendor_arguments_are_refused_not_guessed() {
    let dialect = openai_dialect();
    for (name, args) in [
        ("apply_patch", json!("not an object")),
        ("apply_patch", json!({"path": "a"})),
        ("apply_patch", json!({"type": "update_file", "path": "a"})),
        (
            "apply_patch",
            json!({"type": "update_file", "path": "a", "diff": "@@\n?x\n"}),
        ),
        ("apply_patch", json!({"type": "rename_file", "path": "a"})),
        (
            "apply_patch",
            json!({"type": "create_file", "path": "", "diff": "+a"}),
        ),
        (
            "apply_patch",
            json!({"type": "create_file", "path": "a", "diff": "+a", "extra": 1}),
        ),
        ("shell", json!({"commands": "ls"})),
        ("shell", json!({"commands": [""]})),
        ("shell", json!({"commands": ["ls"], "environment": {}})),
    ] {
        assert!(
            dialect.translate(name, args.clone()).is_err(),
            "{name} {args}"
        );
    }
}

#[test]
fn a_hunk_that_only_removes_lines_removes_their_line_ends() {
    let translated = openai_dialect()
        .translate(
            "apply_patch",
            json!({"type": "update_file", "path": "a", "diff": " keep\n-drop\n"}),
        )
        .expect("update");
    assert_eq!(translated.args["old_string"], "keep\ndrop");
    assert_eq!(translated.args["new_string"], "keep");
    let only_removed = openai_dialect()
        .translate(
            "apply_patch",
            json!({"type": "update_file", "path": "a", "diff": "@@\n-drop\n"}),
        )
        .expect("update");
    assert_eq!(only_removed.args["old_string"], "drop\n");
    assert_eq!(only_removed.args["new_string"], "");
}

#[test]
fn calls_with_no_patch_form_stay_as_stored_in_history() {
    let dialect = openai_dialect();
    // replace_all and an empty replacement have no patch form; a directory
    // for the shell has none either.
    assert!(
        dialect
            .wire_call(
                "edit_file",
                &json!({"file_path":"a","old_string":"x","new_string":"y","replace_all":true})
            )
            .is_none()
    );
    assert!(
        dialect
            .wire_call(
                "edit_file",
                &json!({"file_path":"a","old_string":"x","new_string":""})
            )
            .is_none()
    );
    assert!(
        dialect
            .wire_call("run_command", &json!({"command":"ls","directory":"src"}))
            .is_none()
    );
    // Other tools show as themselves.
    assert_eq!(
        dialect.wire_call("calendar_list", &json!({"day":"d"})),
        Some(("calendar_list".to_owned(), json!({"day":"d"})))
    );
}

// ---- strict nulls through the registry --------------------------------------

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

fn frozen(dialect: Dialect) -> ToolRegistry {
    let received = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    for definition in canonical() {
        registry
            .register(Arc::new(Recorder {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    let rendered = dialect.render(&registry.definitions());
    assert!(registry.freeze_presented(rendered, vec![], dialect));
    registry
}

#[test]
fn null_for_an_optional_argument_is_left_out_of_the_canonical_call() {
    let registry = frozen(openai_dialect());
    let translated = registry
        .translate("calendar_list", json!({"day": "d", "limit": null}))
        .expect("translates");
    assert_eq!(translated.name, "calendar_list");
    assert_eq!(translated.args, json!({"day": "d"}));
    assert!(translated.args_changed);
    // A value stays; a call with nothing to drop is not marked changed.
    let kept = registry
        .translate("calendar_list", json!({"day": "d", "limit": 3}))
        .expect("translates");
    assert_eq!(
        (kept.args, kept.args_changed),
        (json!({"day": "d", "limit": 3}), false)
    );
    // A required argument sent as null is not dropped: validation refuses it.
    let required = registry
        .translate("calendar_list", json!({"day": null}))
        .expect("translates");
    assert_eq!(required.args, json!({"day": null}));
    assert!(
        !registry
            .validate_input("calendar_list", &required.args)
            .is_empty()
    );
}

#[test]
fn built_in_calls_reach_the_canonical_tool_through_the_registry() {
    let registry = frozen(openai_dialect());
    let patch = registry
        .translate(
            "apply_patch",
            json!({"type":"create_file","path":"a.md","diff":"+hi\n"}),
        )
        .expect("translates");
    assert_eq!(patch.name, "write_file");
    assert!(registry.validate_input(&patch.name, &patch.args).is_empty());
    let edit = registry
        .translate(
            "apply_patch",
            json!({"type":"update_file","path":"a.md","diff":"@@\n-hi\n+yo\n"}),
        )
        .expect("translates");
    assert_eq!(edit.name, "edit_file");
    assert!(registry.validate_input(&edit.name, &edit.args).is_empty());
    let shell = registry
        .translate("shell", json!({"commands":["ls"],"timeout_ms":5000}))
        .expect("translates");
    assert_eq!(shell.name, "run_command");
    assert!(registry.validate_input(&shell.name, &shell.args).is_empty());
}

#[test]
fn the_chat_variant_reads_null_arguments_for_common_wire_names() {
    let registry = frozen(openai_chat_dialect());
    let translated = registry
        .translate(
            "edit",
            json!({"file_path":"a","old_string":"x","new_string":"y","replace_all":null}),
        )
        .expect("translates");
    assert_eq!(translated.name, "edit_file");
    assert_eq!(
        translated.args,
        json!({"file_path":"a","old_string":"x","new_string":"y"})
    );
}

#[test]
fn history_shows_patch_and_shell_calls_by_their_vendor_names() {
    let dialect = openai_dialect();
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = vec![
        ToolCall {
            id: "c1".into(),
            name: "edit_file".into(),
            arguments_json: r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#.into(),
        },
        ToolCall {
            id: "c2".into(),
            name: "run_command".into(),
            arguments_json: r#"{"command":"ls"}"#.into(),
        },
    ];
    let mut r1 = ChatMessage::text("tool", "{}");
    r1.tool_call_id = Some("c1".into());
    r1.name = Some("edit_file".into());
    let shown = present_history(&dialect, vec![assistant, r1]);
    assert_eq!(shown[0].tool_calls[0].name, "apply_patch");
    assert_eq!(shown[0].tool_calls[1].name, "shell");
    let patch: Value = serde_json::from_str(&shown[0].tool_calls[0].arguments_json).unwrap();
    assert_eq!(patch["type"], "update_file");
    assert_eq!(shown[1].name.as_deref(), Some("apply_patch"));
}
