//! Backend-neutral search types and the [`SearchBackend`] port.

use async_trait::async_trait;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: u8,
    pub freshness: Option<Freshness>,
    pub include_domains: Vec<String>,
    pub language: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub url: Url,
    pub title: String,
    pub snippet: String,
    pub published: Option<String>,
    pub extra: Vec<String>,
    pub engine: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendId {
    SearXng,
    Brave,
    Exa,
    /// Keyless default backend; the vendor behind it is not exposed.
    Builtin,
}

impl BackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SearXng => "searxng",
            Self::Brave => "brave",
            Self::Exa => "exa",
            Self::Builtin => "builtin",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub backend: BackendId,
    pub suggestions: Vec<String>,
    pub answer: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProbeReport {
    pub latency_ms: u64,
    pub result_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("backend rejected the credentials")]
    Unauthorized,
    #[error("backend refused JSON output")]
    FormatDisabled,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("engines unavailable: {}", .0.join(", "))]
    EnginesUnavailable(Vec<String>),
    #[error("quota exhausted")]
    QuotaExhausted,
    #[error("request timed out")]
    Timeout,
    #[error("backend unavailable (HTTP {0})")]
    Unavailable(u16),
    #[error("network error: {0}")]
    Network(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("cancelled")]
    Cancelled,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The backend refused this request (HTTP 400/422); another may accept it.
    #[error("backend rejected the request: {0}")]
    Rejected(String),
}

impl BackendError {
    /// Whether the failure is transient (a retry may succeed).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::EnginesUnavailable(_)
                | Self::QuotaExhausted
                | Self::Timeout
                | Self::Unavailable(_)
                | Self::Network(_)
                | Self::BadResponse(_)
        )
    }
}

impl BackendError {
    /// Errors that end the fallback chain: caller cancelled, or our own bad
    /// input that no backend could accept.
    pub fn stops_fallback(&self) -> bool {
        matches!(self, Self::Cancelled | Self::InvalidRequest(_))
    }
}

#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn id(&self) -> BackendId;
    async fn search(
        &self,
        req: &SearchRequest,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse, BackendError>;
    async fn probe(&self) -> Result<ProbeReport, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(BackendError::RateLimited { retry_after: None }.is_retryable());
        assert!(BackendError::EnginesUnavailable(vec![]).is_retryable());
        assert!(BackendError::QuotaExhausted.is_retryable());
        assert_eq!(BackendError::QuotaExhausted.to_string(), "quota exhausted");
        assert!(BackendError::Timeout.is_retryable());
        assert!(BackendError::Unavailable(503).is_retryable());
        assert!(BackendError::Network("x".into()).is_retryable());
        assert!(BackendError::BadResponse("x".into()).is_retryable());
        assert!(!BackendError::Unauthorized.is_retryable());
        assert!(!BackendError::FormatDisabled.is_retryable());
        assert!(!BackendError::Cancelled.is_retryable());
        assert!(!BackendError::InvalidRequest("x".into()).is_retryable());
        assert!(!BackendError::Rejected("x".into()).is_retryable());
        assert_eq!(
            BackendError::InvalidRequest("x".into()).to_string(),
            "invalid request: x"
        );
        assert!(BackendError::Cancelled.stops_fallback());
        assert!(BackendError::InvalidRequest("x".into()).stops_fallback());
        assert!(!BackendError::Unauthorized.stops_fallback());
        assert!(!BackendError::Rejected("x".into()).stops_fallback());
        assert!(!BackendError::QuotaExhausted.stops_fallback());
    }
}
