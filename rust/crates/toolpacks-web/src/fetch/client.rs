//! HTTP client for `web_fetch`: manual redirects, content-type allowlist,
//! streamed body cap. DNS pinning lives in [`super::ssrf::GuardedResolver`].

use std::sync::Arc;

use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::ssrf::{BlockedHost, GuardedResolver, HostResolver, check_url};
use super::{FetchConfig, FetchError};

const ACCEPT_VALUE: &str = "text/html,application/xhtml+xml,text/plain,text/markdown,application/json,application/xml;q=0.9,*/*;q=0.1";
const ALLOWED_TYPES: [&str; 7] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
    "application/json",
    "application/xml",
    "text/xml",
];
const SNIFF_BYTES: usize = 512;

#[derive(Debug)]
pub struct FetchedBody {
    pub final_url: Url,
    /// Lowercased media type without parameters.
    pub content_type: String,
    pub charset: Option<String>,
    pub bytes: Vec<u8>,
    pub truncated_body: bool,
}

pub struct SafeHttpClient {
    http: reqwest::Client,
    cfg: FetchConfig,
}

/// True when a redirect would leave https for http.
pub fn is_downgrade(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() == "http"
}

fn map_error(error: reqwest::Error) -> FetchError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(err) = source {
        if let Some(blocked) = err.downcast_ref::<BlockedHost>() {
            return FetchError::Blocked(blocked.0.clone());
        }
        source = err.source();
    }
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Network(error.without_url().to_string())
    }
}

/// Splits a Content-Type header into (media type, charset).
fn parse_content_type(raw: &str) -> (String, Option<String>) {
    let mut parts = raw.split(';');
    let mime = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').to_owned())
    });
    (mime, charset)
}

/// Media type safe to echo to a model: the part before `;`, at most 64 chars,
/// printable ASCII only (anything else becomes `?`).
pub(crate) fn sanitize_mime(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(64)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

fn sniff(bytes: &[u8]) -> &'static str {
    let head = &bytes[..bytes.len().min(SNIFF_BYTES)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    if lower.contains("<html") || lower.contains("<!doctype html") {
        "text/html"
    } else {
        "text/plain"
    }
}

impl SafeHttpClient {
    pub fn new(cfg: FetchConfig, resolver: Arc<dyn HostResolver>) -> Result<Self, FetchError> {
        let guarded = GuardedResolver::new(resolver, cfg.extra_allowed_ips.clone());
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(cfg.timeout)
            .connect_timeout(cfg.timeout)
            .user_agent(cfg.user_agent.clone())
            .dns_resolver(Arc::new(guarded))
            .build()
            .map_err(|e| FetchError::Network(e.without_url().to_string()))?;
        Ok(Self { http, cfg })
    }

    pub fn config(&self) -> &FetchConfig {
        &self.cfg
    }

    pub async fn get(
        &self,
        url: Url,
        cancel: &CancellationToken,
    ) -> Result<FetchedBody, FetchError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(FetchError::Cancelled),
            result = tokio::time::timeout(
                self.cfg.total_timeout,
                self.get_following_redirects(url),
            ) => result.unwrap_or(Err(FetchError::Timeout)),
        }
    }

    async fn get_following_redirects(&self, start: Url) -> Result<FetchedBody, FetchError> {
        let mut current = start;
        let mut redirects = 0u8;
        loop {
            check_url(&current, &self.cfg)?;
            let resp = self
                .http
                .get(current.clone())
                .header(ACCEPT_ENCODING, "identity")
                .header(ACCEPT, ACCEPT_VALUE)
                .send()
                .await
                .map_err(map_error)?;
            let status = resp.status();
            let location = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok());
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) && location.is_some() {
                if redirects >= self.cfg.max_redirects {
                    return Err(FetchError::TooManyRedirects);
                }
                redirects += 1;
                let next = current
                    .join(location.unwrap_or_default())
                    .map_err(|_| FetchError::InvalidUrl("invalid redirect target.".into()))?;
                if is_downgrade(&current, &next) {
                    return Err(FetchError::Blocked("redirect downgrade".into()));
                }
                current = next;
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::Http(status.as_u16()));
            }
            return self.read_body(resp, current).await;
        }
    }

    async fn read_body(
        &self,
        mut resp: reqwest::Response,
        final_url: Url,
    ) -> Result<FetchedBody, FetchError> {
        let header = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(parse_content_type);
        if let Some((mime, _)) = &header
            && !mime.is_empty()
            && !ALLOWED_TYPES.contains(&mime.as_str())
        {
            return Err(FetchError::UnsupportedType(sanitize_mime(mime)));
        }
        let cap = self.cfg.max_body_bytes;
        let mut bytes = Vec::new();
        let mut truncated_body = false;
        while let Some(chunk) = resp.chunk().await.map_err(map_error)? {
            let room = cap - bytes.len();
            if chunk.len() > room {
                bytes.extend_from_slice(&chunk[..room]);
                truncated_body = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let (content_type, charset) = match header {
            Some((mime, charset)) if !mime.is_empty() => (mime, charset),
            _ => (sniff(&bytes).to_owned(), None),
        };
        Ok(FetchedBody {
            final_url,
            content_type,
            charset,
            bytes,
            truncated_body,
        })
    }
}
