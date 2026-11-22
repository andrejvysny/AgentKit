//! Pack configuration. Secrets live in `Zeroizing` and never reach `Debug`.

use std::fmt;
use std::time::Duration;
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::backend::BackendId;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(14);
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const EXA_ENDPOINT: &str = "https://api.exa.ai/search";

#[derive(Clone, Debug)]
pub struct WebPackConfig {
    /// Ordered: `[0]` is the active backend, the rest are fallbacks.
    pub backends: Vec<BackendConfig>,
    /// Per backend attempt.
    pub request_timeout: Duration,
    /// Per tool call, across all attempts.
    pub total_timeout: Duration,
    pub max_body_bytes: usize,
    pub limits: RunLimits,
    pub cache: CacheConfig,
    /// `None` leaves `web_fetch` out of the pack.
    pub fetch: Option<crate::fetch::FetchConfig>,
}

impl WebPackConfig {
    pub fn new(backends: Vec<BackendConfig>) -> Self {
        Self {
            backends,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            limits: RunLimits::default(),
            cache: CacheConfig::default(),
            fetch: Some(crate::fetch::FetchConfig::default()),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.backends.is_empty() {
            return Err(ConfigError::NoBackends);
        }
        let mut seen: Vec<&'static str> = Vec::new();
        for backend in &self.backends {
            let id = backend.id().as_str();
            if seen.contains(&id) {
                return Err(ConfigError::DuplicateBackend(id));
            }
            seen.push(id);
        }
        let zero = [
            (self.request_timeout.is_zero(), "request_timeout"),
            (self.total_timeout.is_zero(), "total_timeout"),
            (self.max_body_bytes == 0, "max_body_bytes"),
            (self.limits.searches_per_run == 0, "searches_per_run"),
            (self.cache.entries_per_run == 0, "cache.entries_per_run"),
            (self.cache.max_runs == 0, "cache.max_runs"),
        ];
        if let Some((_, name)) = zero.into_iter().find(|(bad, _)| *bad) {
            return Err(ConfigError::InvalidLimit(name));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    pub searches_per_run: u32,
    pub fetches_per_run: u32,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            searches_per_run: 8,
            fetches_per_run: 12,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CacheConfig {
    pub ttl: Duration,
    pub entries_per_run: usize,
    pub max_runs: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(600),
            entries_per_run: 32,
            max_runs: 64,
        }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum BackendConfig {
    SearXng(SearXngConfig),
    Brave(BraveConfig),
    Exa(ExaConfig),
}

impl BackendConfig {
    pub fn id(&self) -> BackendId {
        match self {
            Self::SearXng(_) => BackendId::SearXng,
            Self::Brave(_) => BackendId::Brave,
            Self::Exa(_) => BackendId::Exa,
        }
    }
}

#[derive(Clone)]
pub struct SearXngConfig {
    pub base_url: Url,
    pub auth: SearXngAuth,
    pub categories: String,
    /// 0 off, 1 moderate, 2 strict.
    pub safesearch: u8,
    pub language: Option<String>,
}

impl fmt::Debug for SearXngConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut url = self.base_url.clone();
        let _ = url.set_username("");
        let _ = url.set_password(None);
        f.debug_struct("SearXngConfig")
            .field("base_url", &url.as_str())
            .field("auth", &self.auth)
            .field("categories", &self.categories)
            .field("safesearch", &self.safesearch)
            .field("language", &self.language)
            .finish()
    }
}

impl SearXngConfig {
    pub fn new(base_url: Url) -> Self {
        Self {
            base_url,
            auth: SearXngAuth::None,
            categories: "general".into(),
            safesearch: 1,
            language: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let url = &self.base_url;
        let ok = matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.query().is_none();
        if !ok {
            // The URL is not echoed: a rejected URL may carry userinfo.
            return Err(ConfigError::InvalidBaseUrl(
                "base_url must be http(s) without userinfo, query or fragment".into(),
            ));
        }
        if self.safesearch > 2 {
            return Err(ConfigError::InvalidSafesearch(self.safesearch));
        }
        let has_auth = !matches!(self.auth, SearXngAuth::None);
        if has_auth && url.scheme() == "http" && !is_local_host(url) {
            return Err(ConfigError::InsecureAuth("searxng"));
        }
        Ok(())
    }
}

/// Loopback, private/link-local IP literals and conventional local names.
fn is_local_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost"
                || [".local", ".lan", ".internal", ".home.arpa"]
                    .iter()
                    .any(|suffix| d.ends_with(suffix))
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Some(Host::Ipv6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
        None => false,
    }
}

#[derive(Clone)]
pub enum SearXngAuth {
    None,
    Basic {
        username: String,
        password: Zeroizing<String>,
    },
    Header {
        name: String,
        value: Zeroizing<String>,
    },
}

impl fmt::Debug for SearXngAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Header { name, .. } => f
                .debug_struct("Header")
                .field("name", name)
                .field("value", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BraveSafesearch {
    Off,
    #[default]
    Moderate,
    Strict,
}

#[derive(Clone)]
pub struct BraveConfig {
    pub api_key: Zeroizing<String>,
    pub endpoint: Url,
    pub country: Option<String>,
    pub safesearch: BraveSafesearch,
}

impl BraveConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Zeroizing::new(api_key.into()),
            endpoint: default_endpoint(BRAVE_ENDPOINT),
            country: None,
            safesearch: BraveSafesearch::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        validate_key(&self.api_key, "brave")?;
        validate_endpoint(&self.endpoint)
    }
}

impl fmt::Debug for BraveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BraveConfig")
            .field("api_key", &"<redacted>")
            .field("endpoint", &redacted_url(&self.endpoint))
            .field("country", &self.country)
            .field("safesearch", &self.safesearch)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExaSearchType {
    #[default]
    Auto,
    Fast,
    Neural,
}

impl ExaSearchType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Neural => "neural",
        }
    }
}

#[derive(Clone)]
pub struct ExaConfig {
    pub api_key: Zeroizing<String>,
    pub endpoint: Url,
    pub search_type: ExaSearchType,
}

impl ExaConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Zeroizing::new(api_key.into()),
            endpoint: default_endpoint(EXA_ENDPOINT),
            search_type: ExaSearchType::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        validate_key(&self.api_key, "exa")?;
        validate_endpoint(&self.endpoint)
    }
}

impl fmt::Debug for ExaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExaConfig")
            .field("api_key", &"<redacted>")
            .field("endpoint", &redacted_url(&self.endpoint))
            .field("search_type", &self.search_type)
            .finish()
    }
}

fn default_endpoint(raw: &str) -> Url {
    // Only called with the compile-time constants above, which parse.
    Url::parse(raw).unwrap_or_else(|e| unreachable!("static endpoint {raw}: {e}"))
}

fn redacted_url(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

fn validate_key(key: &str, name: &'static str) -> Result<(), ConfigError> {
    if key.trim().is_empty() {
        return Err(ConfigError::MissingApiKey(name));
    }
    Ok(())
}

/// https required; plain http only for loopback hosts (local test stubs).
fn validate_endpoint(url: &Url) -> Result<(), ConfigError> {
    let loopback = match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme_ok || url.username() != "" || url.password().is_some() {
        return Err(ConfigError::InvalidBaseUrl(
            "endpoint must be https (http only for loopback) without userinfo".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no search backends configured")]
    NoBackends,
    #[error("backend configured more than once: {0}")]
    DuplicateBackend(&'static str),
    #[error("missing api key for {0}")]
    MissingApiKey(&'static str),
    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),
    #[error("safesearch must be 0, 1 or 2, got {0}")]
    InvalidSafesearch(u8),
    #[error("invalid auth header name: {0}")]
    InvalidHeaderName(String),
    #[error("{0} credentials would be sent unencrypted; use https or a local address.")]
    InsecureAuth(&'static str),
    #[error("{0} must be greater than zero")]
    InvalidLimit(&'static str),
    #[error("http client setup failed: {0}")]
    Http(String),
}

#[cfg(test)]
mod tests;
