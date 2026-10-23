use super::{Problem, providers::connection_error};
use agentkit_connections::{Capability, ConnectionConfig, Model};
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn create(body: &Value, allow_http: bool) -> Result<ConnectionConfig, Problem> {
    Ok(ConnectionConfig {
        label: body["label"].as_str().unwrap().into(),
        provider_kind: body["kind"].as_str().unwrap().into(),
        endpoint: endpoint(&body["baseUrl"])?,
        account: None,
        credential_required: body.get("apiKey").is_some(),
        extra_header_names: names(body),
        allow_insecure_http: allow_http,
        enabled: body.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        capabilities: BTreeSet::from([
            Capability::Chat,
            Capability::ModelDiscovery,
            Capability::Probe,
        ]),
        models: vec![model(body["defaultModel"].as_str().unwrap())],
    })
}

pub(super) fn update(
    mut config: ConnectionConfig,
    body: &Value,
    allow_http: bool,
) -> Result<ConnectionConfig, Problem> {
    config.allow_insecure_http = allow_http;
    if body.get("extraHeaders").is_some() {
        config.extra_header_names = names(body);
    }
    if let Some(value) = body.get("label").and_then(Value::as_str) {
        config.label = value.into();
    }
    if let Some(value) = body.get("kind").and_then(Value::as_str) {
        config.provider_kind = value.into();
    }
    if let Some(value) = body.get("baseUrl") {
        config.endpoint = endpoint(value)?;
    }
    if let Some(value) = body.get("enabled").and_then(Value::as_bool) {
        config.enabled = value;
    }
    if body.get("apiKey").is_some() {
        config.credential_required = true;
    }
    if let Some(id) = body.get("defaultModel").and_then(Value::as_str)
        && !config.models.iter().any(|model| model.id == id)
    {
        config.models.push(model(id));
    }
    Ok(config)
}

fn endpoint(value: &Value) -> Result<reqwest::Url, Problem> {
    value
        .as_str()
        .unwrap()
        .parse()
        .map_err(|_| connection_error(agentkit_connections::ConnectionError::InvalidConfiguration))
}

fn model(id: &str) -> Model {
    Model {
        id: id.into(),
        display_name: None,
        capabilities: BTreeSet::from([Capability::Chat]),
        context_window_tokens: None,
    }
}

fn names(body: &Value) -> BTreeSet<String> {
    body.get("extraHeaders")
        .and_then(Value::as_object)
        .map(|headers| headers.keys().cloned().collect())
        .unwrap_or_default()
}
