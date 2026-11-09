use agentkit_core::{AuthKind, ConnectionIdentity, CoreError};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
/// Hosts resolve credentials for this immutable identity, never global selection.
#[async_trait]
pub trait TrustedTransport: Send + Sync {
    fn check_current(&self) -> Result<(), CoreError> {
        Ok(())
    }
    /// Durable transports may perform storage checks here without blocking the
    /// runtime thread. Delivery and persistence use the same authority fence.
    async fn check_authority(&self) -> Result<(), CoreError> {
        self.check_current()
    }
    fn identity(&self) -> ConnectionIdentity;
    fn auth_kind(&self) -> AuthKind;
    /// Custom transports that await consent or credentials must override this and
    /// check the guard after those waits, immediately before their network send.
    async fn request_guarded(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        guard: Option<std::sync::Arc<dyn agentkit_core::DispatchGuard>>,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        if let Some(guard) = guard {
            guard.check().await?;
        }
        self.request(identity, operation, body, cancellation).await
    }
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
    dispatch_guarded(transport, identity, operation, body, None, cancellation).await
}

pub(crate) async fn dispatch_guarded(
    transport: &dyn TrustedTransport,
    identity: &ConnectionIdentity,
    operation: &str,
    body: Option<Value>,
    guard: Option<std::sync::Arc<dyn agentkit_core::DispatchGuard>>,
    cancellation: CancellationToken,
) -> Result<reqwest::Response, CoreError> {
    match dispatch_status(transport, identity, operation, body, guard, cancellation).await? {
        Ok(response) => Ok(response),
        Err((status, payload)) => Err(failure(status, &payload)),
    }
}

/// Like [`dispatch_guarded`], but hands a non-success status back with its
/// (bounded, best-effort) JSON payload so the caller may retry.
pub(crate) async fn dispatch_status(
    transport: &dyn TrustedTransport,
    identity: &ConnectionIdentity,
    operation: &str,
    body: Option<Value>,
    guard: Option<std::sync::Arc<dyn agentkit_core::DispatchGuard>>,
    cancellation: CancellationToken,
) -> Result<Result<reqwest::Response, (u16, Value)>, CoreError> {
    transport.check_authority().await?;
    if transport.identity() != *identity {
        return Err(CoreError::responses("connection_changed"));
    }
    let response = tokio::select! { biased;
        _=cancellation.cancelled()=>return Err(CoreError::new("cancelled","aborted")),
        value=transport.request_guarded(identity,operation,body,guard,cancellation.clone())=>value?,
    };
    if response.status().is_success() {
        return Ok(Ok(response));
    }
    let status = response.status().as_u16();
    let payload = read_json(response, cancellation)
        .await
        .unwrap_or(Value::Null);
    Ok(Err((status, payload)))
}

pub(crate) fn failure(status: u16, payload: &Value) -> CoreError {
    let code = classify(payload["error"]["code"].as_str().unwrap_or(""));
    let code = if code != "provider_error" {
        code
    } else {
        match status {
            401 => "authentication_required",
            403 => "consent_required",
            429 => "usage_limit",
            503 => "service_unavailable",
            _ => "provider_error",
        }
    };
    CoreError::responses(code)
}
pub(crate) fn classify(code: &str) -> &'static str {
    match code {
        "subscription_sharing_usage_limit_exceeded" => "usage_limit",
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "service_unavailable"
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
