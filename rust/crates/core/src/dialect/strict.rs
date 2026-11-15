//! Strict function schemas (T3.3, TU-26).
//!
//! OpenAI's strict mode needs every object to list all its properties as
//! required and to close `additionalProperties`; an optional parameter is
//! expressed by letting its type include `null` (read 2026-11-15 from
//! developers.openai.com function-calling and structured-outputs guides).
//! [`strict_schema`] rewrites a canonical schema that way; the canonical tool
//! and its schema never change. A model that sends `null` for an optional
//! parameter is read by [`drop_nulls`], which leaves the parameter out: the
//! canonical call has what the model meant and nothing it did not say.

use serde_json::{json, Value};
use std::collections::BTreeSet;

/// Keywords strict mode accepts. Everything else makes a schema unfit for
/// strict mode (the tool is then offered as it is, non-strict).
const SUPPORTED: [&str; 17] = [
    "type",
    "description",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "pattern",
];
/// Keywords that only restrict or annotate; strict mode does not take them
/// (the guides list no `minLength`, `maxLength` or `default`). They are left
/// out of what the model sees; the canonical tool still enforces them.
const DROPPED: [&str; 9] = [
    "default",
    "minLength",
    "maxLength",
    "uniqueItems",
    "title",
    "format",
    "$schema",
    "$id",
    "$comment",
];

/// `schema` as a strict schema, or `None` when it cannot be one (an open
/// object, a `oneOf`, a keyword outside [`SUPPORTED`]).
pub fn strict_schema(schema: &Value) -> Option<Value> {
    let mut out = schema.clone();
    strictify(&mut out, true).then_some(out)
}

fn strictify(node: &mut Value, root: bool) -> bool {
    let Some(map) = node.as_object_mut() else {
        return false;
    };
    for key in DROPPED {
        map.remove(key);
    }
    if map.keys().any(|key| !SUPPORTED.contains(&key.as_str())) {
        return false;
    }
    if let Some(options) = map.get_mut("anyOf") {
        let Some(options) = options.as_array_mut() else {
            return false;
        };
        return !root && options.iter_mut().all(|option| strictify(option, false));
    }
    let is_object = map.get("type").is_some_and(|kind| kind == "object")
        || map.contains_key("properties");
    if is_object {
        if !matches!(map.get("additionalProperties"), None | Some(Value::Bool(false))) {
            return false;
        }
        // An object with no declared properties is open; only the root of a
        // tool with no parameters may be closed.
        if !map.contains_key("properties") && !root {
            return false;
        }
        let required: BTreeSet<String> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut properties = match map.remove("properties") {
            Some(Value::Object(properties)) => properties,
            Some(_) => return false,
            None => serde_json::Map::new(),
        };
        let names: Vec<String> = properties.keys().cloned().collect();
        for name in &names {
            let child = properties.get_mut(name).expect("listed key");
            if !strictify(child, false) {
                return false;
            }
            if !required.contains(name) {
                make_nullable(child);
            }
        }
        map.insert("properties".to_owned(), Value::Object(properties));
        map.insert("required".to_owned(), json!(names));
        map.insert("additionalProperties".to_owned(), Value::Bool(false));
    }
    if let Some(items) = map.get_mut("items")
        && !strictify(items, false)
    {
        return false;
    }
    true
}

fn is_null_schema(node: &Value) -> bool {
    node.get("type").is_some_and(|kind| kind == "null")
}

/// Lets `node` accept `null` as well as what it accepts.
fn make_nullable(node: &mut Value) {
    if node.is_object() && node.get("type").is_none() && node.get("anyOf").is_none() {
        let inner = std::mem::take(node);
        *node = json!({"anyOf": [inner, {"type": "null"}]});
        return;
    }
    let Some(map) = node.as_object_mut() else {
        return;
    };
    if let Some(options) = map.get_mut("anyOf").and_then(Value::as_array_mut) {
        if !options.iter().any(is_null_schema) {
            options.push(json!({"type": "null"}));
        }
        return;
    }
    match map.get_mut("type") {
        Some(Value::String(kind)) => {
            if kind != "null" {
                let kind = kind.clone();
                map.insert("type".to_owned(), json!([kind, "null"]));
            }
        }
        Some(Value::Array(kinds)) => {
            if !kinds.iter().any(|kind| kind == "null") {
                kinds.push(json!("null"));
            }
        }
        _ => {}
    }
    if let Some(options) = map.get_mut("enum").and_then(Value::as_array_mut)
        && !options.contains(&Value::Null)
    {
        options.push(Value::Null);
    }
}

/// True when `schema` already is a strict schema: a closed object at every
/// level, every property required, only supported keywords. The request
/// builder sets `strict` on exactly the tools for which this holds.
pub fn is_strict_compliant(schema: &Value) -> bool {
    schema.get("type") == Some(&json!("object")) && compliant(schema, true)
}

fn compliant(node: &Value, root: bool) -> bool {
    let Some(map) = node.as_object() else {
        return false;
    };
    if map.keys().any(|key| !SUPPORTED.contains(&key.as_str())) {
        return false;
    }
    if let Some(options) = map.get("anyOf") {
        return !root
            && options
                .as_array()
                .is_some_and(|options| options.iter().all(|option| compliant(option, false)));
    }
    let is_object = map.get("type").is_some_and(|kind| kind == "object")
        || map.contains_key("properties");
    if is_object {
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            return false;
        }
        let Some(properties) = map.get("properties").and_then(Value::as_object) else {
            return false;
        };
        let required: BTreeSet<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if properties.len() != required.len()
            || !properties.keys().all(|name| required.contains(name.as_str()))
            || !properties.values().all(|child| compliant(child, false))
        {
            return false;
        }
    }
    map.get("items").is_none_or(|items| compliant(items, false))
}

/// `example` with `null` for every property of `schema` it leaves out: a
/// strict schema needs every property present.
pub(super) fn fill_nulls(example: &Value, schema: &Value) -> Value {
    match (example, schema.get("properties").and_then(Value::as_object)) {
        (Value::Object(given), Some(properties)) => {
            let mut out = given.clone();
            for (name, child) in properties {
                match out.get(name) {
                    Some(value) => {
                        let filled = fill_nulls(value, child);
                        out.insert(name.clone(), filled);
                    }
                    None => {
                        out.insert(name.clone(), Value::Null);
                    }
                }
            }
            Value::Object(out)
        }
        _ => example.clone(),
    }
}

fn allows_null(schema: &Value) -> bool {
    let typed = match schema.get("type") {
        Some(Value::String(kind)) => kind == "null",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "null"),
        _ => false,
    };
    typed
        || schema
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|options| options.iter().any(is_null_schema))
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|options| options.contains(&Value::Null))
}

/// Removes, from `value`, every `null` that stands for an optional property
/// of the canonical `schema` that does not itself accept null. Returns true
/// when anything was removed. A required property, or one the schema allows
/// to be null, is left as sent.
pub fn drop_nulls(value: &mut Value, schema: &Value) -> bool {
    match value {
        Value::Object(map) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let required: BTreeSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut changed = false;
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let Some(child_schema) = properties.and_then(|known| known.get(&key)) else {
                    continue;
                };
                let is_null = map.get(&key).is_some_and(Value::is_null);
                if is_null {
                    if !required.contains(key.as_str()) && !allows_null(child_schema) {
                        map.remove(&key);
                        changed = true;
                    }
                } else if let Some(child) = map.get_mut(&key) {
                    changed |= drop_nulls(child, child_schema);
                }
            }
            changed
        }
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items") else {
                return false;
            };
            items
                .iter_mut()
                .fold(false, |changed, item| drop_nulls(item, item_schema) | changed)
        }
        _ => false,
    }
}
