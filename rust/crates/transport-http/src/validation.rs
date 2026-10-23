use crate::Problem;
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn body_schema(operation: &str) -> Option<&'static str> {
    operation
        .parse::<agentkit_contracts::RestOperation>()
        .ok()?
        .request_schema()
}

pub(crate) fn validate_body(operation: &str, body: &Value) -> Result<(), Problem> {
    if let Some(schema) = body_schema(operation) {
        agentkit_contracts::validate_schema(schema, body)
            .map_err(|error| Problem::new(400, "invalid_request", error.to_string()))?;
        semantic(operation, body)?;
    }
    Ok(())
}

fn semantic(operation: &str, body: &Value) -> Result<(), Problem> {
    let nonempty = match operation {
        "forkChat" => &["fromMessageId"][..],
        "applyProposal" => &["operationId"][..],
        "createProvider" => &["label", "kind", "baseUrl", "defaultModel"][..],
        "grantAllowance" => &["toolName", "proposalKind"][..],
        "createMcpServer" => &["alias"][..],
        _ => &[],
    };
    for field in nonempty {
        if body
            .get(*field)
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(invalid(format!("`{field}` must not be empty.")));
        }
    }
    if operation == "createProvider"
        && let Some(id) = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    {
        provider_id(id.trim())?;
    }
    if operation == "updateSettings"
        && let Some(count) = body.get("maxToolIterations")
        && agentkit_contracts::javascript_number(count)
            .is_none_or(|n| !n.is_finite() || !(1.0..=64.0).contains(&n) || n.fract() != 0.0)
    {
        return Err(invalid(
            "`maxToolIterations` must be an integer between 1 and 64.",
        ));
    }
    if matches!(operation, "createMcpServer" | "updateMcpServer") {
        mcp_semantic(body)?;
    }
    Ok(())
}

pub(crate) fn provider_id(id: &str) -> Result<(), Problem> {
    if id.len() > 64
        || id.is_empty()
        || id.chars().all(|c| c == '.')
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(invalid(
            "`id` must match ^[A-Za-z0-9._-]{1,64}$ and must not be only dots.",
        ));
    }
    Ok(())
}

pub(crate) fn validate_query(
    operation: &str,
    query: &BTreeMap<String, String>,
) -> Result<(), Problem> {
    if matches!(
        operation,
        "listChats" | "listMessages" | "searchMessages" | "listToolEvents" | "listProposals"
    ) && let Some(raw) = query.get("limit")
        && positive_integer(raw).is_none_or(|v| v > 1000)
    {
        return Err(invalid(
            "Query parameter `limit` must be an integer between 1 and 1000.",
        ));
    }
    if operation == "searchMessages" && !query.contains_key("q") {
        return Err(invalid("Query parameter `q` is required."));
    }
    if operation == "listMessages"
        && let Some(cursor) = query.get("cursor")
        && cursor
            .strip_prefix('m')
            .and_then(|s| u64::from_str_radix(s, 36).ok())
            .is_none()
    {
        return Err(invalid(
            "Query parameter `cursor` is not in the form this server issues.",
        ));
    }
    Ok(())
}

fn invalid(detail: impl Into<String>) -> Problem {
    Problem::new(400, "invalid_request", detail)
}

fn mcp_semantic(body: &Value) -> Result<(), Problem> {
    if let Some(alias) = body.get("alias").and_then(Value::as_str) {
        let alias = alias.trim();
        if !alias.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !alias
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(invalid("`alias` must match ^[a-z][a-z0-9-]*$."));
        }
    }
    if let Some(transport) = body.get("transport") {
        let field = if transport["kind"] == "stdio" {
            "command"
        } else {
            "url"
        };
        if transport[field]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(invalid(format!("`transport.{field}` must not be empty.")));
        }
    }
    if let Some(options) = body.get("resilience").and_then(Value::as_object) {
        for (key, value) in options {
            let (min, max, integer) = match key.as_str() {
                "requestTimeoutMs"
                | "connectTimeoutMs"
                | "connectBackoffBaseMs"
                | "connectBackoffMaxMs"
                | "circuitOpenMs" => (0.0, 600_000.0, true),
                "maxConnectAttempts" | "reconnectMaxAttempts" => (1.0, 20.0, true),
                "reconnectBackoffFactor" => (1.0, 20.0, false),
                "retryTimeouts" if value.is_boolean() => continue,
                _ => {
                    return Err(invalid(format!(
                        "`resilience.{key}` is not a known resilience option."
                    )));
                }
            };
            if agentkit_contracts::javascript_number(value).is_none_or(|n| {
                !n.is_finite() || n < min || n > max || (integer && n.fract() != 0.0)
            }) {
                return Err(invalid(format!(
                    "`resilience.{key}` must be between {min} and {max}."
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn positive_integer(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let number = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()? as f64
    } else if let Some(octal) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
        u64::from_str_radix(octal, 8).ok()? as f64
    } else if let Some(binary) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
        u64::from_str_radix(binary, 2).ok()? as f64
    } else {
        raw.parse::<f64>().ok()?
    };
    (number.is_finite() && (1.0..=1000.0).contains(&number) && number.fract() == 0.0)
        .then_some(number as u64)
}
