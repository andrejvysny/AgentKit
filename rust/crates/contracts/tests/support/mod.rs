use agentkit_contracts::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub fn fixture_root() -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = root.join("../../fixtures");
    if workspace.exists() {
        workspace
    } else {
        root.join("tests/fixtures")
    }
}

pub fn fixture(name: &str) -> Value {
    let bundled = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_root().join(name)).unwrap()).unwrap();
    let bundled: Value = serde_json::from_str(&std::fs::read_to_string(bundled).unwrap()).unwrap();
    assert_eq!(value, bundled, "packaged fixture drift: {name}");
    value
}

pub fn example(schema: &Value) -> Value {
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    if let Some(arms) = schema.get("anyOf") {
        return example(&arms[0]);
    }
    match schema["type"].as_str() {
        Some("object") => {
            let mut object = serde_json::Map::new();
            for name in schema["required"].as_array().into_iter().flatten() {
                let name = name.as_str().unwrap();
                object.insert(name.into(), example(&schema["properties"][name]));
            }
            Value::Object(object)
        }
        Some("array") => Value::Array(
            (0..schema["minItems"].as_u64().unwrap_or(0))
                .map(|_| example(&schema["items"]))
                .collect(),
        ),
        Some("string") => match schema["pattern"].as_str() {
            Some("^https?://") => json!("https://example.com/image.png"),
            Some("^[a-f0-9]{64}$") => json!("a".repeat(64)),
            Some(MEDIA_TYPE_PATTERN) => json!("image/png"),
            _ => json!("x".repeat(schema["minLength"].as_u64().unwrap_or(1) as usize)),
        },
        Some("number" | "integer") => json!(schema["minimum"].as_f64().unwrap_or(0.0)),
        Some("boolean") => json!(false),
        _ => Value::Null,
    }
}

pub fn roundtrip<T: DeserializeOwned + serde::Serialize + JsonDeserialize>(name: &str) {
    let value = example(&schemas()[name]);
    validate_schema(name, &value).unwrap_or_else(|error| panic!("{name}: {error}"));
    let typed: T =
        serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{name}: {error}"));
    let output = serde_json::to_value(typed).unwrap();
    assert_eq!(canonical_json(&output), canonical_json(&value), "{name}");
    let enriched = private_example(&schemas()[name]);
    validate_schema(name, &enriched).unwrap_or_else(|error| panic!("{name}: {error}"));
    let decoded = T::from_json(enriched.clone()).unwrap_or_else(|error| panic!("{name}: {error}"));
    assert_eq!(
        canonical_json(&serde_json::to_value(decoded).unwrap()),
        canonical_json(&enriched),
        "private keys in {name}"
    );
}

fn private_example(schema: &Value) -> Value {
    let marker = || {
        parse_javascript_json(r#"{"$serde_json::private::Number":"123","nested":{"$serde_json::private::RawValue":"true"}}"#).unwrap()
    };
    if schema.as_object().is_some_and(|object| object.is_empty()) {
        return marker();
    }
    if schema.get("const").is_some() {
        return example(schema);
    }
    if let Some(arms) = schema["anyOf"].as_array() {
        return private_example(arms.iter().find(|arm| arm["type"] != "null").unwrap());
    }
    match schema["type"].as_str() {
        Some("object") => {
            let mut object = serde_json::Map::new();
            for (name, child) in schema["properties"].as_object().into_iter().flatten() {
                object.insert(name.clone(), private_example(child));
            }
            if schema["additionalProperties"] != false {
                let extension = schema["patternProperties"]
                    .as_object()
                    .and_then(|patterns| patterns.values().next())
                    .or_else(|| {
                        schema["additionalProperties"]
                            .as_object()
                            .map(|_| &schema["additionalProperties"])
                    });
                object.insert(
                    "extension".into(),
                    extension.map_or_else(marker, private_example),
                );
            }
            Value::Object(object)
        }
        Some("array") => Value::Array(
            (0..schema["minItems"].as_u64().unwrap_or(1))
                .map(|_| private_example(&schema["items"]))
                .collect(),
        ),
        _ => example(schema),
    }
}
