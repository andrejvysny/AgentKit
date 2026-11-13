use super::{McpConfigError, Ports};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

fn prefix(app_id: &str, id: &str) -> String {
    let encode = |text: &str| {
        text.as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    format!("mcp/{}/{}/", encode(app_id), encode(id))
}

fn owned(reference: &str, scope: &str) -> bool {
    reference
        .strip_prefix(scope)
        .and_then(|tail| tail.strip_prefix("secret_"))
        .is_some_and(|token| {
            token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn same_target(current: &Value, previous: &Value) -> bool {
    let target = if current["kind"] == "http" {
        "url"
    } else {
        "command"
    };
    current["kind"] == previous["kind"] && current[target] == previous[target]
}

fn values_key(value: &Value) -> Result<&'static str, McpConfigError> {
    match value["transport"]["kind"].as_str() {
        Some("http") => {
            let url = reqwest::Url::parse(
                value["transport"]["url"]
                    .as_str()
                    .ok_or(McpConfigError::Invalid)?,
            )
            .map_err(|_| McpConfigError::Invalid)?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(McpConfigError::Invalid);
            }
            Ok("headers")
        }
        Some("stdio") => Ok("env"),
        _ => Err(McpConfigError::Invalid),
    }
}

struct Prepared {
    field: &'static str,
    scope: String,
    refs: Map<String, Value>,
    values: Vec<(String, String)>,
}

fn prepare(
    app_id: &str,
    value: &Value,
    previous: Option<&Value>,
    ports: &Ports,
) -> Result<Prepared, McpConfigError> {
    let field = values_key(value)?;
    let scope = prefix(app_id, value["id"].as_str().ok_or(McpConfigError::Invalid)?);
    let refs = value["secretRefs"].as_object().cloned().unwrap_or_default();
    if (ports.secrets.is_some() || ports.live.is_some())
        && refs.values().any(|reference| {
            !reference
                .as_str()
                .is_some_and(|reference| owned(reference, &scope))
        })
    {
        return Err(McpConfigError::Invalid);
    }
    let old = previous.filter(|old| same_target(&value["transport"], &old["transport"]));
    let values = value["transport"][field]
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(key, supplied)| {
            let actual = if supplied == "***" {
                old.and_then(|old| old["transport"][field].get(&key))
                    .and_then(Value::as_str)
                    .ok_or(McpConfigError::Invalid)?
                    .to_owned()
            } else {
                supplied.as_str().ok_or(McpConfigError::Invalid)?.to_owned()
            };
            for token in placeholders(&actual)? {
                if !refs.contains_key(&token) {
                    return Err(McpConfigError::Invalid);
                }
            }
            Ok((key, actual))
        })
        .collect::<Result<Vec<_>, McpConfigError>>()?;
    Ok(Prepared {
        field,
        scope,
        refs,
        values,
    })
}

async fn materialize(
    prepared: &mut Prepared,
    ports: &Ports,
) -> Result<(Map<String, Value>, Vec<String>), McpConfigError> {
    let mut next = Map::new();
    let mut staged = Vec::new();
    for (key, actual) in &prepared.values {
        if actual.contains("${") {
            next.insert(key.clone(), json!(actual));
            continue;
        }
        let Some(secrets) = &ports.secrets else {
            cleanup(&staged, ports).await;
            return Err(McpConfigError::SecretStore);
        };
        let token = format!("secret_{}", uuid::Uuid::new_v4().simple());
        let reference = format!("{}{token}", prepared.scope);
        staged.push(reference.clone());
        if secrets.set(&reference, actual).await.is_err() {
            cleanup(&staged, ports).await;
            return Err(McpConfigError::SecretStore);
        }
        prepared.refs.insert(token.clone(), json!(reference));
        next.insert(key.clone(), json!(format!("${{{token}}}")));
    }
    Ok((next, staged))
}

pub(super) async fn normalize(
    app_id: &str,
    mut value: Value,
    previous: Option<&Value>,
    ports: &Ports,
) -> Result<(Value, Vec<String>), McpConfigError> {
    let mut prepared = prepare(app_id, &value, previous, ports)?;
    let (next, staged) = materialize(&mut prepared, ports).await?;
    let mut used = BTreeSet::new();
    for actual in next.values().filter_map(Value::as_str) {
        used.extend(placeholders(actual)?);
    }
    prepared.refs.retain(|token, _| used.contains(token));
    if value["transport"].get(prepared.field).is_some() {
        value["transport"][prepared.field] = Value::Object(next);
    }
    if !prepared.refs.is_empty() || value.get("secretRefs").is_some() {
        value["secretRefs"] = Value::Object(prepared.refs);
    }
    Ok((value, staged))
}

pub(super) async fn cleanup(references: &[String], ports: &Ports) {
    if let Some(secrets) = &ports.secrets {
        for reference in references {
            let _ = secrets.delete(reference).await;
        }
    }
}

pub(super) async fn retire(app_id: &str, previous: &Value, current: &Value, ports: &Ports) {
    let scope = prefix(app_id, previous["id"].as_str().unwrap_or_default());
    let retained: BTreeSet<_> = current["secretRefs"]
        .as_object()
        .into_iter()
        .flat_map(|refs| refs.values())
        .filter_map(Value::as_str)
        .collect();
    let removed = previous["secretRefs"]
        .as_object()
        .into_iter()
        .flat_map(|refs| refs.values())
        .filter_map(Value::as_str)
        .filter(|reference| owned(reference, &scope) && !retained.contains(reference))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    cleanup(&removed, ports).await;
}

pub(super) fn redact(value: &mut Value) {
    for field in ["env", "headers"] {
        if let Some(values) = value["transport"]
            .get_mut(field)
            .and_then(Value::as_object_mut)
        {
            for value in values.values_mut() {
                *value = json!("***");
            }
        }
    }
}

pub(super) fn validate_declarations(records: &[super::McpServerDto]) -> Result<(), McpConfigError> {
    for record in records {
        let value = serde_json::to_value(record).map_err(|_| McpConfigError::Invalid)?;
        let field = values_key(&value)?;
        if value["transport"][field]
            .as_object()
            .into_iter()
            .flat_map(|values| values.values())
            .any(|value| !value.as_str().is_some_and(|value| value.contains("${")))
        {
            return Err(McpConfigError::SecretStore);
        }
    }
    Ok(())
}

fn placeholders(mut text: &str) -> Result<Vec<String>, McpConfigError> {
    let mut tokens = Vec::new();
    while let Some((_, tail)) = text.split_once("${") {
        let (token, rest) = tail.split_once('}').ok_or(McpConfigError::Invalid)?;
        if token.is_empty() || token.contains('{') {
            return Err(McpConfigError::Invalid);
        }
        tokens.push(token.to_owned());
        text = rest;
    }
    Ok(tokens)
}
