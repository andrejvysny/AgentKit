use crate::{ConnectionStatus, ManagedConnectionConfig, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConnectionRecord {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "connectorId")]
    pub connector_id: String,
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(
        rename = "workspaceId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub workspace_id: Optional<String>,
    #[serde(rename = "revision", serialize_with = "crate::serialize_number")]
    pub revision: f64,
    #[serde(rename = "config")]
    pub config: ManagedConnectionConfig,
    #[serde(rename = "status")]
    pub status: ConnectionStatus,
    #[serde(
        rename = "errorCode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub error_code: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ConnectionRecord, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        connector_id: crate::required_field(&mut object, "connectorId")?,
        account_id: crate::required_field(&mut object, "accountId")?,
        workspace_id: crate::optional_field(&mut object, "workspaceId")?,
        revision: crate::required_field(&mut object, "revision")?,
        config: crate::required_field(&mut object, "config")?,
        status: crate::required_field(&mut object, "status")?,
        error_code: crate::optional_field(&mut object, "errorCode")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
