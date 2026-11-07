use crate::McpClientError;
use serde_json::{Map, Value};
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct Redaction(Mutex<Vec<String>>);

impl Redaction {
    pub(crate) fn remember(&self, value: &Value) -> Result<(), McpClientError> {
        let mut secrets = self.0.lock().unwrap_or_else(|error| error.into_inner());
        for path in [
            "/secret",
            "/token_response/access_token",
            "/token_response/refresh_token",
        ] {
            let Some(secret) = value
                .pointer(path)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if !secrets.iter().any(|known| known == secret) {
                if secret.len() > 65_536 || secrets.len() >= 1024 {
                    return Err(McpClientError::AuthRequired);
                }
                secrets.push(secret.to_owned());
            }
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        Ok(())
    }
    pub(crate) fn text(&self, raw: &str) -> Result<String, McpClientError> {
        let value =
            agentkit_contracts::parse_javascript_json(raw).map_err(|_| McpClientError::Request)?;
        let secrets = self.0.lock().unwrap_or_else(|error| error.into_inner());
        serde_json::to_string(&scrub(&value, &secrets)?).map_err(|_| McpClientError::Request)
    }
}

fn scrub(value: &Value, secrets: &[String]) -> Result<Value, McpClientError> {
    let text = |value: &str| {
        secrets.iter().fold(value.to_owned(), |value, secret| {
            value.replace(secret, "[REDACTED]")
        })
    };
    Ok(match value {
        Value::String(value) => Value::String(text(value)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| scrub(value, secrets))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => {
            let mut output = Map::new();
            for (key, value) in values {
                if output.insert(text(key), scrub(value, secrets)?).is_some() {
                    return Err(McpClientError::Request);
                }
            }
            Value::Object(output)
        }
        _ => value.clone(),
    })
}
