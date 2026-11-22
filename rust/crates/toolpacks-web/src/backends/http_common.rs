//! HTTP plumbing shared by every search backend adapter.

use std::time::Duration;

use reqwest::{Client, Response, redirect::Policy};

use crate::backend::BackendError;
use crate::config::ConfigError;

const USER_AGENT: &str = "AgentKit-WebSearch/0.1";

pub(super) fn build_client(timeout: Duration) -> Result<Client, ConfigError> {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| ConfigError::Http(e.without_url().to_string()))
}

pub(super) fn too_large() -> BackendError {
    BackendError::BadResponse("response too large".into())
}

pub(super) fn map_transport(error: reqwest::Error) -> BackendError {
    if error.is_timeout() {
        return BackendError::Timeout;
    }
    BackendError::Network(error.without_url().to_string())
}

/// Reads the body in chunks, failing as soon as it exceeds `max_bytes`.
pub(super) async fn read_capped(
    mut resp: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, BackendError> {
    if resp.content_length().is_some_and(|n| n > max_bytes as u64) {
        return Err(too_large());
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(map_transport)? {
        if body.len() + chunk.len() > max_bytes {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 2xx passes; `specific` maps backend-specific codes first, then the shared
/// 429 / 5xx / other mapping applies.
pub(super) fn check_status(
    resp: &Response,
    specific: impl Fn(u16) -> Option<BackendError>,
) -> Result<(), BackendError> {
    let code = resp.status().as_u16();
    if (200..=299).contains(&code) {
        return Ok(());
    }
    if let Some(error) = specific(code) {
        return Err(error);
    }
    match code {
        429 => {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            Err(BackendError::RateLimited { retry_after })
        }
        500..=599 => Err(BackendError::Unavailable(code)),
        _ => Err(BackendError::BadResponse(format!("HTTP {code}"))),
    }
}

/// Status mapping common to key-authenticated JSON APIs (Brave, Exa).
pub(super) fn keyed_api_status(
    name: &'static str,
    invalid: &'static [u16],
) -> impl Fn(u16) -> Option<BackendError> {
    move |code| match code {
        401 | 403 => Some(BackendError::Unauthorized),
        402 => Some(BackendError::QuotaExhausted),
        c if invalid.contains(&c) => Some(BackendError::Rejected(format!(
            "{name} rejected the request (HTTP {c})"
        ))),
        _ => None,
    }
}
