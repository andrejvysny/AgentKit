//! Strict tool-schema vocabulary and resource identity checks shared with Ajv.
use crate::ValidationError;
use jsonschema::Draft;
use serde_json::Value;
use std::collections::BTreeSet;

pub(crate) fn draft(schema: &Value) -> Result<Draft, ValidationError> {
    match schema.get("$schema").and_then(Value::as_str) {
        None
        | Some("http://json-schema.org/draft-07/schema#")
        | Some("https://json-schema.org/draft-07/schema") => Ok(Draft::Draft7),
        Some("https://json-schema.org/draft/2019-09/schema") => Ok(Draft::Draft201909),
        Some("https://json-schema.org/draft/2020-12/schema") => Ok(Draft::Draft202012),
        Some(uri) => Err(invalid(format!("Unsupported JSON Schema dialect: {uri}"))),
    }
}

#[derive(Default)]
struct Profile {
    identities: BTreeSet<String>,
    unevaluated_properties: bool,
    complex_patterns: bool,
}

pub(crate) fn check(schema: &Value) -> Result<(), ValidationError> {
    let dialect = draft(schema)?;
    let mut profile = Profile::default();
    walk(
        schema,
        dialect,
        "https://agentkit-schema.invalid/root",
        &mut profile,
    )?;
    if dialect != Draft::Draft7 && profile.unevaluated_properties && profile.complex_patterns {
        return Err(invalid(
            "Native unevaluatedProperties requires literal patternProperties patterns",
        ));
    }
    Ok(())
}

fn walk(
    schema: &Value,
    dialect: Draft,
    base: &str,
    profile: &mut Profile,
) -> Result<(), ValidationError> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    let identity = schema
        .get("$id")
        .and_then(Value::as_str)
        .map(|id| resolve_id(base, id))
        .transpose()?;
    if let Some(identity) = &identity
        && !profile.identities.insert(identity.clone())
    {
        return Err(invalid(format!("Ambiguous schema identity: {identity}")));
    }
    let base = identity.as_deref().unwrap_or(base);
    for (name, child) in object {
        if !known_keyword(dialect, name) {
            return Err(invalid(format!("Unknown tool schema keyword: {name}")));
        }
        profile.unevaluated_properties |= name == "unevaluatedProperties";
        if name == "patternProperties"
            && let Some(patterns) = child.as_object()
        {
            profile.complex_patterns |= patterns.keys().any(|pattern| !literal_pattern(pattern));
        }
        check_children(name, child, dialect, base, profile)?;
    }
    Ok(())
}

fn check_children(
    name: &str,
    child: &Value,
    dialect: Draft,
    base: &str,
    profile: &mut Profile,
) -> Result<(), ValidationError> {
    match name {
        "properties" | "patternProperties" | "definitions" | "$defs" | "dependentSchemas"
        | "dependencies" => {
            if let Some(children) = child.as_object() {
                for child in children.values() {
                    walk(child, dialect, base, profile)?;
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
        | "oneOf"
        | "prefixItems"
        | "unevaluatedProperties"
        | "unevaluatedItems"
        | "contentSchema" => {
            if let Some(children) = child.as_array() {
                for child in children {
                    walk(child, dialect, base, profile)?;
                }
            } else {
                walk(child, dialect, base, profile)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// The dependency's unevaluated-property tracker bypasses custom ECMAScript matching.
// Literal patterns have identical matching rules in both engines.
fn literal_pattern(pattern: &str) -> bool {
    let pattern = pattern.strip_prefix('^').unwrap_or(pattern);
    let pattern = pattern.strip_suffix('$').unwrap_or(pattern);
    pattern
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_:/-".contains(&byte))
}

fn known_keyword(draft: Draft, name: &str) -> bool {
    if name == "prefixItems" {
        return draft == Draft::Draft202012;
    }
    draft.is_known_keyword(name)
        || matches!(
            name,
            "$comment"
                | "$async"
                | "definitions"
                | "$defs"
                | "title"
                | "description"
                | "default"
                | "examples"
                | "readOnly"
                | "writeOnly"
                | "deprecated"
                | "nullable"
        )
        || (draft != Draft::Draft7
            && matches!(
                name,
                "$vocabulary" | "contentEncoding" | "contentMediaType" | "contentSchema"
            ))
}

fn resolve_id(base: &str, id: &str) -> Result<String, ValidationError> {
    fluent_uri::UriRef::parse(id)
        .map_err(invalid)?
        .resolve_against(&fluent_uri::Uri::parse(base).map_err(invalid)?)
        .map_err(invalid)
        .map(|uri| uri.normalize().to_string())
}

fn invalid(error: impl std::fmt::Display) -> ValidationError {
    ValidationError::Invalid(error.to_string())
}
