use agentkit_contracts::{compile_schema, compile_tool_schema};
use serde_json::json;

#[test]
fn patterns_and_additional_properties_share_ecmascript_matching() {
    let validator = compile_tool_schema(&json!({
        "type":"object", "properties":{"x":{"minimum":2}},
        "patternProperties":{"^.$":{"type":"integer"},"^x":{"maximum":4}},
        "additionalProperties":{"type":"string"}
    }))
    .unwrap();
    assert!(validator.is_valid(&json!({"x":3,"\r":"line","\u{2028}":"line","\u{2029}":"line"})));
    assert!(!validator.is_valid(&json!({"x":1})));
    assert!(!validator.is_valid(&json!({"x":5})));
    assert!(!validator.is_valid(&json!({"x":3.5})));
    assert!(!validator.is_valid(&json!({"\r":1})));
    assert!(!validator.is_valid(&json!({"y":"wrong"})));
}

#[test]
fn property_children_resolve_local_references_with_root_ids() {
    for id in [
        None,
        Some("https://schemas.example.test/root.json"),
        Some("urn:agentkit:schema-root"),
        Some("urn:agentkit:validator"),
        Some("https://agentkit-compiled.invalid/0/root.json"),
        Some("folder/root.json"),
        Some(""),
    ] {
        let mut schema = json!({
            "definitions":{"positive":{"type":"integer","minimum":1}},
            "patternProperties":{"^x/[~]é$":{"$ref":"#/definitions/positive"}},
            "properties":{"known":{"$ref":"#/definitions/positive"}},
            "additionalProperties":{"$ref":"#/definitions/positive"}
        });
        if let Some(id) = id {
            schema["$id"] = json!(id);
        }
        let validator = compile_schema(&schema).unwrap();
        assert!(validator.is_valid(&json!({"x/~é":1,"known":2,"extra":3})));
        for name in ["x/~é", "known", "extra"] {
            let mut invalid = json!({});
            invalid[name] = json!(0);
            assert!(!validator.is_valid(&invalid), "{name}");
        }
    }
}

#[test]
fn recursive_property_references_follow_instance_depth() {
    let validator = compile_tool_schema(&json!({
        "$id":"https://schemas.example.test/tree.json", "type":"object",
        "properties":{"name":{"type":"string"}},
        "patternProperties":{"^child$":{"$ref":"#"}}, "additionalProperties":false
    }))
    .unwrap();
    assert!(validator.is_valid(&json!({"name":"root","child":{"name":"leaf"}})));
    assert!(!validator.is_valid(&json!({"child":{"name":1}})));
    assert!(!validator.is_valid(&json!({"child":{"unknown":true}})));
}

#[test]
fn property_children_retain_nested_resource_reference_scope() {
    let validator = compile_schema(&json!({
        "$id":"https://schemas.example.test/root.json",
        "definitions":{"positive":{"$id":"positive.json","type":"integer","minimum":1}},
        "patternProperties":{"^branch$":{
            "$id":"branch.json", "definitions":{"text":{"type":"string"}},
            "patternProperties":{"^text$":{"$ref":"#/definitions/text"},"^count$":{"$ref":"positive.json"}},
            "additionalProperties":false
        }}, "additionalProperties":false
    })).unwrap();
    assert!(validator.is_valid(&json!({"branch":{"text":"ok","count":1}})));
    assert!(!validator.is_valid(&json!({"branch":{"text":1}})));
    assert!(!validator.is_valid(&json!({"branch":{"count":0}})));
}

#[test]
fn child_compilation_rejects_remote_refs_invalid_schemas_and_invalid_patterns() {
    for schema in [
        json!({"patternProperties":{"x":{"$ref":"https://example.test/never-fetch"}}}),
        json!({"additionalProperties":{"$ref":"file:///private/tmp/never-read"}}),
        json!({"patternProperties":{"x":{"type":"invalid"}}}),
        json!({"patternProperties":{"[":{}}}),
        json!({"pattern":"["}),
    ] {
        assert!(compile_tool_schema(&schema).is_err(), "{schema}");
    }
}
