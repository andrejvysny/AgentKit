//! Validation errors from closed object schemas name the property that failed
//! and say why (hardening H1.6, blocker B6; TU-10). The verdict itself is
//! unchanged: these tests only read the error of a call that already fails.
use agentkit_contracts::{compile_schema, compile_tool_schema};
use serde_json::{Value, json};

/// The schema of AgentKit's `memory_propose`, the tool of FINDINGS F3.
fn propose() -> Value {
    json!({
        "type":"object",
        "properties":{
            "text":{"type":"string","minLength":1,"maxLength":600},
            "kind":{"type":"string","enum":["fact","preference","person","routine","lesson","skill_ref"]},
            "scope":{"type":"string","enum":["user","project","agent"]},
            "rationale":{"type":"string","minLength":1,"maxLength":300},
            "owner_requested":{"type":"boolean"}
        },
        "required":["text","kind","scope","rationale"],
        "additionalProperties":false
    })
}

/// Errors as the tool registry renders them for the model.
fn errors(schema: &Value, args: &Value) -> Vec<String> {
    let validator = compile_tool_schema(schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(args)
        .map(|error| format!("{}: {error}", error.instance_path))
        .collect();
    assert_eq!(errors.is_empty(), validator.is_valid(args), "{errors:?}");
    errors
}

fn one(schema: &Value, args: &Value) -> String {
    let errors = errors(schema, args);
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors[0].clone()
}

fn valid() -> Value {
    json!({"text":"Prefers documents in English.","kind":"preference","scope":"user","rationale":"Said so."})
}

/// F3 as recorded: `scope` is outside its enum. Before H1.6 the text was
/// `: property rejected by ECMAScript schema constraints`.
#[test]
fn a_declared_property_with_a_bad_value_is_named_with_the_reason() {
    let mut args = valid();
    args["scope"] = json!("global");
    let error = one(&propose(), &args);
    assert!(error.starts_with("/scope: "), "{error}");
    assert!(error.contains("\"global\""), "{error}");
    assert!(
        error.contains("\"user\""),
        "lists the allowed values: {error}"
    );
    assert!(!error.contains("ECMAScript"), "{error}");
}

#[test]
fn a_declared_property_of_the_wrong_type_is_named() {
    let mut args = valid();
    args["owner_requested"] = json!("yes");
    let error = one(&propose(), &args);
    assert!(error.starts_with("/owner_requested: "), "{error}");
    assert!(error.contains("boolean"), "{error}");
}

#[test]
fn an_extra_property_is_named_with_the_allowed_ones() {
    let mut args = valid();
    args["reason"] = json!("why");
    let error = one(&propose(), &args);
    assert!(error.starts_with("/reason: "), "{error}");
    assert!(error.contains("\"reason\" is not allowed"), "{error}");
    for name in ["text", "kind", "scope", "rationale", "owner_requested"] {
        assert!(error.contains(&format!("\"{name}\"")), "{name}: {error}");
    }
}

#[test]
fn every_extra_property_is_named() {
    let mut args = valid();
    args["reason"] = json!("why");
    args["confidence"] = json!(0.9);
    let error = one(&propose(), &args);
    assert!(
        error.contains("\"reason\", \"confidence\" are not allowed here"),
        "{error}"
    );
    assert!(error.contains("Remove them"), "{error}");
}

#[test]
fn a_nested_failure_carries_its_full_path() {
    let schema = json!({
        "type":"object",
        "properties":{"options":{
            "type":"object",
            "properties":{"depth":{"type":"integer"}},
            "additionalProperties":false
        }},
        "additionalProperties":false
    });
    let error = one(&schema, &json!({"options":{"depth":"deep"}}));
    assert!(error.starts_with("/options/depth: "), "{error}");
    assert!(error.contains("integer"), "{error}");

    let error = one(&schema, &json!({"options":{"level":1}}));
    assert!(error.starts_with("/options/level: "), "{error}");
    assert!(error.contains("\"depth\""), "{error}");
}

#[test]
fn a_property_name_with_pointer_characters_is_escaped_in_the_path() {
    let schema = json!({
        "type":"object",
        "properties":{"a/b":{"type":"object","properties":{"c~d":{"type":"integer"}},"additionalProperties":false}},
        "additionalProperties":false
    });
    let error = one(&schema, &json!({"a/b":{"c~d":"x"}}));
    assert!(error.starts_with("/a~1b/c~0d: "), "{error}");
}

#[test]
fn an_object_with_patterns_names_them_as_allowed() {
    let schema = json!({
        "type":"object",
        "properties":{"name":{"type":"string"}},
        "patternProperties":{"^x-":{"type":"string"}},
        "additionalProperties":false
    });
    let error = one(&schema, &json!({"name":"a","other":1}));
    assert!(error.starts_with("/other: "), "{error}");
    assert!(error.contains("\"name\""), "{error}");
    assert!(error.contains("^x-"), "{error}");
}

#[test]
fn an_object_with_no_properties_says_so() {
    let schema = json!({"type":"object","additionalProperties":false});
    let error = one(&schema, &json!({"any":1}));
    assert!(error.starts_with("/any: "), "{error}");
    assert!(error.contains("takes no properties"), "{error}");
}

#[test]
fn wire_dto_validation_gets_the_same_message() {
    let validator = compile_schema(&propose()).unwrap();
    let mut args = valid();
    args["reason"] = json!("why");
    let error = validator.validate(&args).unwrap_err();
    assert_eq!(error.instance_path.as_str(), "/reason");
    assert!(error.to_string().contains("\"rationale\""), "{error}");
}

/// Each value rule says what the value broke; before H1.6 every one of them
/// read "value rejected by JavaScript numeric schema semantics".
#[test]
fn every_value_rule_says_what_the_value_broke() {
    let schema = json!({
        "type":"object",
        "properties":{
            "kind":{"enum":["a","b"]},
            "fixed":{"const":"x"},
            "count":{"type":"integer","minimum":1,"maximum":9},
            "low":{"type":"number","exclusiveMinimum":0},
            "step":{"type":"integer","multipleOf":5},
            "name":{"type":"string","minLength":2,"maxLength":4},
            "tags":{"type":"array","maxItems":1,"uniqueItems":true},
            "day":{"type":"string","format":"date"}
        },
        "additionalProperties":false
    });
    for (args, path, reason) in [
        (
            json!({"kind":"c"}),
            "/kind",
            "\"c\" is not one of \"a\", \"b\"",
        ),
        (
            json!({"fixed":"y"}),
            "/fixed",
            "\"y\" is not the required value \"x\"",
        ),
        (
            json!({"count":"3"}),
            "/count",
            "\"3\" is not of type \"integer\"",
        ),
        (json!({"count":0}), "/count", "0 is less than the minimum 1"),
        (
            json!({"count":10}),
            "/count",
            "10 is greater than the maximum 9",
        ),
        (json!({"low":0}), "/low", "0 must be greater than 0"),
        (json!({"step":7}), "/step", "7 is not a multiple of 5"),
        (
            json!({"name":"a"}),
            "/name",
            "\"a\" is shorter than 2 characters",
        ),
        (
            json!({"name":"abcde"}),
            "/name",
            "\"abcde\" is longer than 4 characters",
        ),
        (json!({"tags":["a","b"]}), "/tags", "has more than 1 items"),
        (json!({"tags":["a","a"]}), "/tags", "has more than 1 items"),
        (
            json!({"day":"monday"}),
            "/day",
            "\"monday\" is not a valid \"date\"",
        ),
    ] {
        let errors = errors(&schema, &args);
        assert!(
            errors
                .iter()
                .any(|error| error.starts_with(&format!("{path}: ")) && error.contains(reason)),
            "{args}: {errors:?}"
        );
        assert!(
            errors.iter().all(|error| !error.contains("rejected by")),
            "{args}: {errors:?}"
        );
    }
}

#[test]
fn unique_items_says_so() {
    let schema = json!({"type":"array","uniqueItems":true});
    let error = one(&schema, &json!([1, 1]));
    assert!(error.contains("not unique"), "{error}");
}

#[test]
fn a_long_value_is_cut_in_the_message() {
    let schema =
        json!({"type":"object","properties":{"n":{"type":"integer"}},"additionalProperties":false});
    let error = one(&schema, &json!({"n":"x".repeat(500)}));
    assert!(error.len() < 200, "{error}");
    assert!(error.contains("..."), "{error}");
}
