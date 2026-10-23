use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP {status} [{code}]: {detail}")]
    Http {
        status: u16,
        code: String,
        detail: String,
        problem: Value,
    },
    #[error("Transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Delivery uncertain; retry with the same idempotency key `{key}`: {cause}")]
    UncertainDelivery { key: String, cause: String },
    #[error("Stream protocol error: {0}")]
    Protocol(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) async fn checked(response: reqwest::Response) -> Result<reqwest::Response, ClientError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let problem = agentkit_contracts::parse_javascript_json(&text).unwrap_or(Value::String(text));
    let code = problem
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("http_{status}"));
    let detail = problem
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("HTTP request failed")
        .to_owned();
    Err(ClientError::Http {
        status,
        code,
        detail,
        problem,
    })
}
