//! Preserve meta-schema validation despite jsonschema's finite-number assumption.
use crate::javascript_number;
use serde_json::{Value, json};

pub(crate) fn tool_schema_mirror(schema: &Value) -> (Value, String) {
    let mut suffix = 0;
    let marker = loop {
        let candidate = format!("x-agentkit-infinite-keywords-{suffix}");
        if !contains_key(schema, &candidate) {
            break candidate;
        }
        suffix += 1;
    };
    let mut mirror = schema.clone();
    prepare_schema(&mut mirror, &marker, "");
    (mirror, marker)
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key) || object.values().any(|value| contains_key(value, key))
        }
        Value::Array(items) => items.iter().any(|value| contains_key(value, key)),
        _ => false,
    }
}

fn prepare_schema(schema: &mut Value, marker: &str, path: &str) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let mut overrides = serde_json::Map::new();
    overrides.insert("location".into(), json!(path));
    for name in [
        "multipleOf",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(number) = object.get(name).and_then(javascript_number)
            && number.is_infinite()
        {
            // The private meta-validator assumes finite numeric keyword values.
            // A signed finite mirror retains positivity/nonnegative checks;
            // custom keywords restore infinity after meta-validation.
            object.insert(
                name.into(),
                json!(if number.is_sign_negative() { -1 } else { 1 }),
            );
            overrides.insert(name.into(), json!(true));
        }
    }
    if !overrides.is_empty() {
        object.insert(marker.into(), Value::Object(overrides));
    }
    for (name, value) in object.iter_mut() {
        prepare_child(name, value, marker, &pointer(path, name));
    }
}

fn prepare_child(name: &str, value: &mut Value, marker: &str, path: &str) {
    match name {
        "properties" | "patternProperties" | "definitions" | "$defs" | "dependentSchemas" => {
            if let Some(children) = value.as_object_mut() {
                for (name, child) in children.iter_mut() {
                    prepare_schema(child, marker, &pointer(path, name));
                }
            }
        }
        "dependencies" => {
            if let Some(children) = value.as_object_mut() {
                for (name, child) in children.iter_mut().filter(|(_, child)| child.is_object()) {
                    prepare_schema(child, marker, &pointer(path, name));
                }
            }
        }
        "additionalProperties"
        | "additionalItems"
        | "items"
        | "contains"
        | "propertyNames"
        | "not"
        | "if"
        | "then"
        | "else"
        | "allOf"
        | "anyOf"
        | "oneOf" => {
            if let Some(children) = value.as_array_mut() {
                for (index, child) in children.iter_mut().enumerate() {
                    prepare_schema(child, marker, &pointer(path, &index.to_string()));
                }
            } else {
                prepare_schema(value, marker, path);
            }
        }
        _ => {}
    }
}

pub(crate) fn pointer(parent: &str, segment: &str) -> String {
    format!("{parent}/{}", segment.replace('~', "~0").replace('/', "~1"))
}
