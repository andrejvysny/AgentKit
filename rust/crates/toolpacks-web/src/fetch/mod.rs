//! `web_fetch` support: SSRF-safe HTTP client and HTML extraction (AK-3).

use std::net::IpAddr;
use std::time::Duration;

pub mod client;
pub mod extract;
mod links;
pub mod page;
mod plain;
pub mod ssrf;

pub use crate::tools::fetch::WebFetchTool;
pub use client::{FetchedBody, SafeHttpClient};
pub use extract::ExtractedPage;
pub use ssrf::{GuardedResolver, HostResolver, SystemResolver};

#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Per-request timeout.
    pub timeout: Duration,
    /// Budget for the whole fetch, redirects included.
    pub total_timeout: Duration,
    pub max_body_bytes: usize,
    pub max_redirects: u8,
    pub allowed_ports: Vec<u16>,
    /// Host/test opt-in exemptions from the blocked ranges (e.g. a local stub).
    pub extra_allowed_ips: Vec<IpAddr>,
    pub user_agent: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(18),
            max_body_bytes: 3 * 1024 * 1024,
            max_redirects: 5,
            allowed_ports: vec![80, 443],
            extra_allowed_ips: Vec::new(),
            user_agent: "AgentKit-WebFetch/0.1".into(),
        }
    }
}

/// Per-run fetch budget supplied by the host.
pub trait FetchBudget: Send + Sync {
    /// Returns false when the run has no fetches left.
    fn try_consume_fetch(&self, run_id: &str) -> bool;
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("unsupported content type: {0}")]
    UnsupportedType(String),
    #[error("http status {0}")]
    Http(u16),
    #[error("timeout")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("cancelled")]
    Cancelled,
    #[error("too many redirects")]
    TooManyRedirects,
}
