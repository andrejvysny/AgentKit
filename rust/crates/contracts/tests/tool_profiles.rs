use agentkit_contracts::compile_tool_schema;
use serde_json::json;

#[test]
fn tool_dialects_enforce_their_validation_keywords() {
    for dialect in [
        "https://json-schema.org/draft/2019-09/schema",
        "https://json-schema.org/draft/2020-12/schema",
    ] {
        let schema = json!({"$schema":dialect,"type":"object",
            "properties":{"name":{"type":"string"}},"unevaluatedProperties":false});
        let validator = compile_tool_schema(&schema).unwrap();
        assert!(validator.is_valid(&json!({"name":"ok"})));
        assert!(!validator.is_valid(&json!({"other":1})));
    }
    let validator = compile_tool_schema(&json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"array",
        "prefixItems":[{"type":"string"}], "items":false
    }))
    .unwrap();
    assert!(validator.is_valid(&json!(["ok"])));
    assert!(!validator.is_valid(&json!([1])));
    assert!(!validator.is_valid(&json!(["ok", 1])));
}

#[test]
fn unknown_tool_keywords_formats_and_dialects_are_rejected() {
    for schema in [
        json!({"unknownKeyword":true}),
        json!({"properties":{"x":{"format":"unknown-host-format"}}}),
        json!({"$schema":"https://example.test/unknown"}),
        json!({"prefixItems":[{"type":"string"}]}),
    ] {
        assert!(compile_tool_schema(&schema).is_err(), "{schema}");
    }
}

#[test]
fn schema_metadata_payloads_are_preserved_without_interpreting_them_as_schemas() {
    let validator = compile_tool_schema(&json!({
        "type":"object", "default":{"$schema":"user data", "unknownKeyword":true},
        "const":{"$id":"unchanged", "$schema":"unchanged"}
    }))
    .unwrap();
    assert!(validator.is_valid(&json!({"$id":"unchanged", "$schema":"unchanged"})));
    assert!(!validator.is_valid(&json!({})));
}

#[test]
fn tool_property_children_retain_nested_resource_scope() {
    let validator = compile_tool_schema(&json!({
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
fn tool_schema_ids_are_isolated_between_compilers_but_unique_within_one_document() {
    for field in ["first", "second"] {
        compile_tool_schema(&json!({"$id":"https://schemas.example.test/shared.json",
            "properties":{field:{"type":"string"}}}))
        .unwrap();
    }
    assert!(
        compile_tool_schema(&json!({"properties":{
            "first":{"$id":"same.json","type":"string"},
            "second":{"$id":"same.json","type":"number"}
        }}))
        .is_err()
    );
}

#[test]
fn newer_dialect_literal_patterns_retain_evaluated_property_validation() {
    for dialect in [
        "https://json-schema.org/draft/2019-09/schema",
        "https://json-schema.org/draft/2020-12/schema",
    ] {
        let validator = compile_tool_schema(&json!({"$schema":dialect,
            "type":"object", "patternProperties":{"^x$":{"type":"integer"}},
            "unevaluatedProperties":false
        }))
        .unwrap();
        assert!(validator.is_valid(&json!({"x":1})));
        assert!(!validator.is_valid(&json!({"x":"wrong"})));
        assert!(!validator.is_valid(&json!({"\r":1})));
        assert!(!validator.is_valid(&json!({"x\n":1})));
    }
}

#[test]
fn newer_dialect_complex_evaluated_patterns_fail_closed_including_references() {
    for dialect in [
        "https://json-schema.org/draft/2019-09/schema",
        "https://json-schema.org/draft/2020-12/schema",
    ] {
        for schema in [
            json!({"$schema":dialect,"patternProperties":{"^.$":{"type":"integer"}},
                "unevaluatedProperties":false}),
            json!({"$schema":dialect,"$defs":{"child":{"patternProperties":{"^.$":{}}}},
                "$ref":"#/$defs/child", "unevaluatedProperties":false}),
            json!({"$schema":dialect,"properties":{"child":{"unevaluatedProperties":false}},
                "patternProperties":{"\\s":{}}}),
        ] {
            let error = match compile_tool_schema(&schema) {
                Ok(_) => panic!("unsupported evaluated patterns compiled"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("requires literal patternProperties")
            );
        }
        compile_tool_schema(&json!({"$schema":dialect,"patternProperties":{"^.$":{}}})).unwrap();
    }
}
