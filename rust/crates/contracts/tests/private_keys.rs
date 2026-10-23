use agentkit_contracts::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn marker() -> Value {
    parse_javascript_json(r#"{"$serde_json::private::Number":"123","nested":{"$serde_json::private::RawValue":"true"}}"#).unwrap()
}

#[test]
fn generated_dto_retains_known_json_fields_flattened_unknowns_and_unions() {
    let value = json!({
        "id":"c","title":"c","archived":false,"metadata":{"payload":marker()},
        "createdAt":"t","updatedAt":"t","extension":[marker()]
    });
    let dto: ChatDto = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(dto.metadata["payload"], marker());
    assert_eq!(dto.extra["extension"][0], marker());
    assert_eq!(serde_json::to_value(dto).unwrap(), value);
    let event = json!({"type":"run.completed","runId":"r","timestamp":"t","contractVersion":CONTRACT_VERSION,"eventId":"e","seq":0,"data":{"iterations":0},"extension":marker()});
    let decoded: AiRunEvent = serde_json::from_value(event.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), event);
}

#[derive(Debug, Deserialize, Serialize)]
struct Fields {
    #[serde(deserialize_with = "deserialize_preserving")]
    value: Value,
    #[serde(deserialize_with = "deserialize_preserving")]
    items: Vec<Value>,
    #[serde(deserialize_with = "deserialize_preserving")]
    optional: Option<Value>,
    #[serde(deserialize_with = "deserialize_preserving")]
    map: BTreeMap<String, Value>,
}

#[test]
fn adapter_preserves_value_containers_from_raw_json_and_existing_values() {
    let source =
        json!({"value":marker(),"items":[marker()],"optional":marker(),"map":{"one":marker()}});
    let decoded: Fields = serde_json::from_value(source.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), source);
    let decoded: Fields = serde_json::from_str(&serde_json::to_string(&source).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), source);
}

#[test]
fn validated_alias_decoder_nullable_and_arbitrary_values_preserve_marker_objects() {
    let value = json!({"payload":marker()});
    let alias: AiJsonSchemaObject =
        deserialize_validated("AiJsonSchemaObjectSchema", value.clone()).unwrap();
    assert_eq!(alias["payload"], marker());
    assert_eq!(Value::from_json(marker()).unwrap(), marker());
    let nullable: Nullable<Value> = serde_json::from_value(marker()).unwrap();
    assert_eq!(nullable, Nullable::Value(marker()));
    let null: Nullable<Value> = serde_json::from_value(Value::Null).unwrap();
    assert_eq!(null, Nullable::Null);
    assert!(serde_json::from_value::<AiProviderContinuationVersion>(marker()).is_err());
}
