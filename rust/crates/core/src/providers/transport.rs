use crate::CoreError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionIdentity {
    pub provider_id: String,
    pub protocol: String,
    pub connection_id: String,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    ChatGptAccount,
}
/// Hosts resolve credentials for this immutable identity, never global selection.
#[async_trait]
pub trait TrustedTransport: Send + Sync {
    fn check_current(&self) -> Result<(), CoreError> {
        Ok(())
    }
    fn identity(&self) -> ConnectionIdentity;
    fn auth_kind(&self) -> AuthKind;
    async fn request(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError>;
}
pub(crate) async fn read_json(
    response: reqwest::Response,
    cancellation: CancellationToken,
) -> Result<Value, CoreError> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    loop {
        let next = tokio::select! { biased;
            _=cancellation.cancelled()=>return Err(CoreError::new("cancelled","aborted")),
            item=stream.next()=>item,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| CoreError::responses("transport_error"))?;
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(CoreError::responses("response_limit_exceeded"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| CoreError::responses("malformed_response"))?;
    agentkit_contracts::parse_javascript_json(text)
        .map_err(|_| CoreError::responses("malformed_response"))
}
pub(crate) async fn dispatch(
    transport: &dyn TrustedTransport,
    identity: &ConnectionIdentity,
    operation: &str,
    body: Option<Value>,
    cancellation: CancellationToken,
) -> Result<reqwest::Response, CoreError> {
    transport.check_current()?;
    if transport.identity() != *identity {
        return Err(CoreError::responses("connection_changed"));
    }
    let response = tokio::select! { biased;
        _=cancellation.cancelled()=>return Err(CoreError::new("cancelled","aborted")),
        value=transport.request(identity,operation,body,cancellation.clone())=>value?,
    };
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let payload = read_json(response, cancellation)
        .await
        .unwrap_or(Value::Null);
    let code = classify(payload["error"]["code"].as_str().unwrap_or(""));
    let code = if code != "provider_error" {
        code
    } else {
        match status {
            401 => "authentication_required",
            403 => "consent_required",
            429 => "usage_limit",
            _ => "provider_error",
        }
    };
    Err(CoreError::responses(code))
}
pub(crate) fn classify(code: &str) -> &'static str {
    match code {
        "subscription_sharing_usage_limit_exceeded" | "subscription_sharing_usage_unavailable" => {
            "usage_limit"
        }
        "subscription_sharing_invalid_user"
        | "invalid_api_key"
        | "token_expired"
        | "authentication_error" => "authentication_required",
        "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context"
        | "subscription_sharing_consent_required"
        | "consent_required"
        | "insufficient_scope" => "consent_required",
        "subscription_sharing_user_not_eligible" => "permission_denied",
        "subscription_sharing_unsupported_capability" => "unsupported_parameter",
        "subscription_sharing_route_not_supported" => "unsupported_route",
        _ => "provider_error",
    }
}
