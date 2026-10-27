use agentkit_contracts::*;
use serde_json::{Value, json};
mod support;
use support::{example, fixture};

#[test]
fn schemas_match_frozen_typescript_exports_and_compile() {
    assert_eq!(
        serde_json::to_value(schemas()).unwrap(),
        fixture("schemas.json")
    );
    assert_eq!(schemas().len(), 100);
    for (name, schema) in schemas() {
        compile_schema(schema).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn all_routes_match_typescript() {
    let frozen = fixture("routes.json");
    let rows = frozen["routes"].as_array().unwrap();
    assert_eq!(REST_ROUTES.len(), 48);
    assert_eq!(rows.len(), REST_ROUTES.len());
    for route in REST_ROUTES {
        let row = rows
            .iter()
            .find(|row| row["operation"] == route.operation.as_str())
            .unwrap();
        assert_eq!(row["method"], route.method.as_str());
        assert_eq!(row["path"], route.path);
        assert_eq!(
            route.operation.as_str().parse::<RestOperation>().unwrap(),
            route.operation
        );
    }
}

#[test]
fn all_event_variants_and_golden_traces_roundtrip() {
    let frozen = fixture("run-events.json");
    let variants = frozen["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 17);
    for variant in variants {
        let value = example(&variant["schema"]);
        let event: AiRunEvent = deserialize_validated("AiRunEventSchema", value.clone()).unwrap();
        assert_eq!(event.event_type(), variant["type"]);
        assert_eq!(
            canonical_json(&serde_json::to_value(event).unwrap()),
            canonical_json(&value)
        );
    }
    let directory = support::fixture_root().join("golden-traces");
    for path in std::fs::read_dir(directory).unwrap() {
        let events: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path.unwrap().path()).unwrap()).unwrap();
        for value in events {
            let event: AiRunEvent =
                deserialize_validated("AiRunEventSchema", value.clone()).unwrap();
            assert_eq!(
                canonical_json(&serde_json::to_value(event).unwrap()),
                canonical_json(&value)
            );
        }
    }
}

#[test]
fn missing_and_null_remain_distinct() {
    let base = json!({"id":"chat","createdAt":"t","updatedAt":"t","archived":false,"metadata":{}});
    let absent: ChatDto = serde_json::from_value(base.clone()).unwrap();
    assert!(absent.active_run_id.is_absent());
    let mut null = base.clone();
    null["activeRunId"] = Value::Null;
    let present: ChatDto = serde_json::from_value(null.clone()).unwrap();
    assert_eq!(present.active_run_id, Optional::Present(Nullable::Null));
    assert_eq!(serde_json::to_value(present).unwrap(), null);
    let mut invalid = base;
    invalid["title"] = Value::Null;
    assert!(serde_json::from_value::<ChatDto>(invalid.clone()).is_err());
    assert!(validate_schema("ChatDtoSchema", &invalid).is_err());
    let model = json!({"providerId":"p","modelId":"m","fetchedAt":"t"});
    assert!(serde_json::from_value::<AiProviderModel>(model).is_err());
    let envelope = json!({"ok":true,"sources":[],"warnings":[],"truncated":false,"limits":{"profile":"small","maxBytes":1},"data":null});
    let typed: AiToolResult = serde_json::from_value(envelope.clone()).unwrap();
    assert_eq!(typed.data, Optional::Present(Value::Null));
    assert_eq!(
        canonical_json(&serde_json::to_value(typed).unwrap()),
        canonical_json(&envelope)
    );
}

#[test]
fn extensible_strings_and_unknown_fields_survive() {
    let value = json!({"id":"p","label":"P","kind":"future-provider","baseUrl":"u","defaultModel":"m","enabled":true,"newField":{"x":1}});
    let provider: ProviderDto = deserialize_validated("ProviderDtoSchema", value.clone()).unwrap();
    assert_eq!(provider.kind, "future-provider");
    assert_eq!(serde_json::to_value(provider).unwrap(), value);
    let metadata = EventMetadata::new("r", "t", "e", 1.0);
    let warning = AiRunEvent::from_parts(
        "run.warning",
        &metadata,
        json!({"code":"future-warning","message":"m","newField":1}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(warning).unwrap()["data"]["newField"],
        1
    );
    assert!(AiRunEvent::from_parts("run.future", &metadata, json!({})).is_err());
}

#[test]
fn draft07_denies_remote_refs_and_supports_local_refs() {
    for uri in ["https://example.com/schema", "file:///etc/passwd"] {
        assert!(compile_schema(&json!({"$ref":uri})).is_err());
    }
    let local = compile_schema(
        &json!({"definitions":{"n":{"type":"integer","minimum":2}},"$ref":"#/definitions/n"}),
    )
    .unwrap();
    assert!(local.is_valid(&json!(2)));
    assert!(!local.is_valid(&json!(1)));
    let constraints = compile_schema(&json!({"allOf":[{"type":"object","required":["x"]},{"properties":{"x":{"oneOf":[{"const":2},{"const":3}]}}}],"additionalProperties":true})).unwrap();
    assert!(constraints.is_valid(&json!({"x":2,"future":true})));
    assert!(!constraints.is_valid(&json!({"x":4})));
}

#[test]
fn actual_typescript_idempotency_vectors_match() {
    let frozen = fixture("idempotency.json");
    for vector in frozen["vectors"].as_array().unwrap() {
        let chat = vector["chatId"].as_str().unwrap();
        let message = vector["messageId"].as_str().unwrap();
        let key = vector["key"].as_str().unwrap();
        assert_eq!(derive_idempotent_task_id(chat, key), vector["submitTaskId"]);
        assert_eq!(
            derive_regenerate_task_id(chat, message, key),
            vector["regenerateTaskId"]
        );
    }
    for vector in frozen["fingerprints"].as_array().unwrap() {
        assert_eq!(
            turn_fingerprint(&vector["input"]),
            vector["fingerprint"],
            "{:?}",
            vector["input"]
        );
    }
    assert_eq!(
        canonical_json(&json!({"x":[1.0,-0.0,1e-6,1e-7,1e20,1e21]})),
        "{\"x\":[1,0,0.000001,1e-7,100000000000000000000,1e+21]}"
    );
}

#[test]
fn content_security_and_closed_discriminators_match_typescript() {
    for source in [
        json!({"kind":"url","url":"file:///etc/passwd"}),
        json!({"kind":"url","url":"javascript:alert(1)"}),
        json!({"kind":"data","base64":"AA==","mediaType":"image/png;evil"}),
        json!({"kind":"future","url":"https://example.com"}),
    ] {
        assert!(validate_schema("AiImageSourceSchema", &source).is_err());
    }
    for source in [
        json!({"kind":"url","url":"https://example.com/image.png"}),
        json!({"kind":"data","base64":"AA==","mediaType":"image/png"}),
        json!({"kind":"ref","ref":"opaque:host-handle"}),
    ] {
        let typed: AiImageSource =
            deserialize_validated("AiImageSourceSchema", source.clone()).unwrap();
        assert_eq!(serde_json::to_value(typed).unwrap(), source);
    }
    assert!(validate_schema("AiMessageContentSchema", &json!([])).is_err());
    assert!(validate_schema("AiMessageContentSchema", &json!("")).is_ok());
    assert!(validate_schema("AiContentPartSchema", &json!({"type":"audio","data":"x"})).is_err());
}

#[test]
fn continuation_is_closed_and_constrained() {
    let schema = &schemas()["AiProviderContinuationSchema"];
    let valid = example(schema);
    let typed: AiProviderContinuation =
        deserialize_validated("AiProviderContinuationSchema", valid.clone()).unwrap();
    assert_eq!(
        canonical_json(&serde_json::to_value(typed).unwrap()),
        canonical_json(&valid)
    );
    for path in ["root", "scope", "negative", "fraction", "digest"] {
        let mut invalid = valid.clone();
        match path {
            "root" => invalid["unknown"] = json!(true),
            "scope" => invalid["scope"]["unknown"] = json!(true),
            "negative" => invalid["messageCount"] = json!(-1),
            "fraction" => invalid["scope"]["generation"] = json!(0.5),
            _ => invalid["messageDigest"] = json!("invalid"),
        }
        assert!(
            validate_schema("AiProviderContinuationSchema", &invalid).is_err(),
            "{path}"
        );
    }
}

#[test]
fn request_validation_uses_route_schema() {
    assert!(validate_request(RestOperation::SubmitMessage, &json!({"content":"hello"})).is_ok());
    assert!(validate_request(RestOperation::SubmitMessage, &json!({"content":null})).is_err());
    assert!(validate_request(RestOperation::ApplyProposal, &json!({"operationId":"op"})).is_ok());
    assert!(validate_request(RestOperation::ApplyProposal, &json!({})).is_err());
    assert!(validate_request(RestOperation::GetVersion, &Value::Null).is_ok());
    assert!(validate_request(RestOperation::GetVersion, &json!({})).is_err());
}

include!("support/generated_roundtrips.rs");

#[test]
fn safe_integral_numbers_preserve_javascript_integer_wire_shape() {
    let event = AiRunEvent::from_parts(
        "run.started",
        &EventMetadata::new("r", "t", "e", 0.0),
        json!({"model":"m","toolCount":0.0}),
    )
    .unwrap();
    let serialized = serde_json::to_value(event).unwrap();
    assert_eq!(serialized["seq"].as_u64(), Some(0));
    assert_eq!(serialized["data"]["toolCount"].as_u64(), Some(0));
    assert_eq!(serde_json::to_string(&serialized["seq"]).unwrap(), "0");
    let settings: UpdateSettingsRequest =
        serde_json::from_value(json!({"maxToolIterations":2.0})).unwrap();
    assert_eq!(
        serde_json::to_value(settings).unwrap()["maxToolIterations"].as_u64(),
        Some(2)
    );
    let metadata = EventMetadata::new("r", "t", "e", f64::INFINITY);
    assert!(serde_json::to_value(metadata).is_err());
}
