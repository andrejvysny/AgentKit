// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManagedConnectionConfigResilience {
    #[serde(
        rename = "requestTimeoutMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub request_timeout_ms: Optional<f64>,
    #[serde(
        rename = "connectTimeoutMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub connect_timeout_ms: Optional<f64>,
    #[serde(
        rename = "maxConnectAttempts",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_connect_attempts: Optional<f64>,
    #[serde(
        rename = "connectBackoffBaseMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub connect_backoff_base_ms: Optional<f64>,
    #[serde(
        rename = "connectBackoffMaxMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub connect_backoff_max_ms: Optional<f64>,
    #[serde(
        rename = "circuitOpenMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub circuit_open_ms: Optional<f64>,
    #[serde(
        rename = "reconnectMaxAttempts",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub reconnect_max_attempts: Optional<f64>,
    #[serde(
        rename = "reconnectBackoffFactor",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub reconnect_backoff_factor: Optional<f64>,
    #[serde(
        rename = "retryTimeouts",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub retry_timeouts: Optional<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ManagedConnectionConfigResilience, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        request_timeout_ms: crate::optional_field(&mut object, "requestTimeoutMs")?,
        connect_timeout_ms: crate::optional_field(&mut object, "connectTimeoutMs")?,
        max_connect_attempts: crate::optional_field(&mut object, "maxConnectAttempts")?,
        connect_backoff_base_ms: crate::optional_field(&mut object, "connectBackoffBaseMs")?,
        connect_backoff_max_ms: crate::optional_field(&mut object, "connectBackoffMaxMs")?,
        circuit_open_ms: crate::optional_field(&mut object, "circuitOpenMs")?,
        reconnect_max_attempts: crate::optional_field(&mut object, "reconnectMaxAttempts")?,
        reconnect_backoff_factor: crate::optional_field(&mut object, "reconnectBackoffFactor")?,
        retry_timeouts: crate::optional_field(&mut object, "retryTimeouts")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManagedConnectionConfigDiscovery {
    #[serde(
        rename = "maxPages",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_pages: Optional<f64>,
    #[serde(
        rename = "maxItems",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_items: Optional<f64>,
    #[serde(
        rename = "maxBytes",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_bytes: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ManagedConnectionConfigDiscovery, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        max_pages: crate::optional_field(&mut object, "maxPages")?,
        max_items: crate::optional_field(&mut object, "maxItems")?,
        max_bytes: crate::optional_field(&mut object, "maxBytes")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ManagedConnectionConfig {
    #[serde(rename = "alias")]
    pub alias: AiProviderKind,
    #[serde(rename = "transport")]
    pub transport: McpTransportDto,
    #[serde(
        rename = "secretRefs",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub secret_refs: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(
        rename = "enabled",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub enabled: Optional<bool>,
    #[serde(
        rename = "toolAliases",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_aliases: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(
        rename = "resilience",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub resilience: Optional<ManagedConnectionConfigResilience>,
    #[serde(
        rename = "discovery",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub discovery: Optional<ManagedConnectionConfigDiscovery>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ManagedConnectionConfig, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        alias: crate::required_field(&mut object, "alias")?,
        transport: crate::required_field(&mut object, "transport")?,
        secret_refs: crate::optional_field(&mut object, "secretRefs")?,
        enabled: crate::optional_field(&mut object, "enabled")?,
        tool_aliases: crate::optional_field(&mut object, "toolAliases")?,
        resilience: crate::optional_field(&mut object, "resilience")?,
        discovery: crate::optional_field(&mut object, "discovery")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
