//! SearXNG JSON backend (`GET {base}/search?format=json`).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::backend::*;
use crate::config::{ConfigError, SearXngAuth, SearXngConfig};

use super::http_common::{build_client, check_status, map_transport, read_capped};
use super::merge::{HitSet, normalize_domains, with_site_operator};
use super::searxng_parse::{Page, parse_page};

const MAX_SUGGESTIONS: usize = 3;
const MAX_UNRESPONSIVE: usize = 5;

pub struct SearXngBackend {
    client: Client,
    endpoint: Url,
    auth: SearXngAuth,
    auth_header: Option<(HeaderName, HeaderValue)>,
    categories: String,
    safesearch: u8,
    language: Option<String>,
    max_body_bytes: usize,
    request_timeout: Duration,
}

impl SearXngBackend {
    pub fn new(
        cfg: SearXngConfig,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let auth_header = match &cfg.auth {
            SearXngAuth::Header { name, value } => Some(header_pair(name, value)?),
            _ => None,
        };
        let client = build_client(request_timeout)?;
        let mut endpoint = cfg.base_url.clone();
        let path = format!("{}/search", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        Ok(Self {
            client,
            endpoint,
            auth: cfg.auth,
            auth_header,
            categories: cfg.categories,
            safesearch: cfg.safesearch,
            language: cfg.language,
            max_body_bytes,
            request_timeout,
        })
    }

    fn page_url(&self, req: &SearchRequest, domains: &[String], page: u8) -> Url {
        let q = with_site_operator(&req.query, domains);
        let mut url = self.endpoint.clone();
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", &q)
                .append_pair("format", "json")
                .append_pair("pageno", &page.to_string())
                .append_pair("categories", &self.categories)
                .append_pair("safesearch", &self.safesearch.to_string());
            if let Some(lang) = req.language.as_ref().or(self.language.as_ref()) {
                pairs.append_pair("language", lang);
            }
            if let Some(f) = req.freshness {
                let range = match f {
                    Freshness::Day => "day",
                    Freshness::Week => "week",
                    Freshness::Month => "month",
                    Freshness::Year => "year",
                };
                pairs.append_pair("time_range", range);
            }
        }
        url
    }

    async fn fetch_page(&self, url: Url) -> Result<Page, BackendError> {
        let mut builder = self.client.get(url);
        builder = match &self.auth {
            SearXngAuth::Basic { username, password } => {
                builder.basic_auth(username, Some(password.as_str()))
            }
            _ => builder,
        };
        if let Some((name, value)) = &self.auth_header {
            builder = builder.header(name.clone(), value.clone());
        }
        let resp = builder.send().await.map_err(map_transport)?;
        check_status(&resp, |code| match code {
            401 => Some(BackendError::Unauthorized),
            403 => Some(BackendError::FormatDisabled),
            _ => None,
        })?;
        let body = read_capped(resp, self.max_body_bytes).await?;
        parse_page(&body)
    }

    async fn run(&self, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let started = Instant::now();
        let domains = normalize_domains(&req.include_domains)?;
        let max = usize::from(req.max_results.max(1));
        let first = self.fetch_page(self.page_url(req, &domains, 1)).await?;
        let mut merged = Merged::default();
        let raw_first = first.raw_count;
        merged.absorb(first, &domains);
        if merged.set.hits.len() < max && raw_first >= 1 {
            // Best effort within half the request budget: page 1 already
            // succeeded, so a failing or slow page 2 only costs extra hits.
            let half = self.request_timeout / 2;
            if let Some(left) = self.request_timeout.checked_sub(started.elapsed())
                && started.elapsed() < half
            {
                let url = self.page_url(req, &domains, 2);
                if let Ok(Ok(second)) = tokio::time::timeout(left, self.fetch_page(url)).await {
                    merged.absorb(second, &domains);
                }
            }
        }
        merged.set.hits.truncate(max);
        if merged.set.hits.is_empty() && !merged.unresponsive.is_empty() {
            return Err(BackendError::EnginesUnavailable(merged.unresponsive));
        }
        Ok(SearchResponse {
            hits: merged.set.hits,
            backend: BackendId::SearXng,
            suggestions: merged.suggestions,
            answer: merged.answer,
        })
    }
}

#[derive(Default)]
struct Merged {
    set: HitSet,
    answer: Option<String>,
    suggestions: Vec<String>,
    unresponsive: Vec<String>,
}

impl Merged {
    fn absorb(&mut self, page: Page, domains: &[String]) {
        self.set.absorb(page.hits, domains);
        if self.answer.is_none() {
            self.answer = page.answer;
        }
        for s in page.suggestions {
            if self.suggestions.len() < MAX_SUGGESTIONS && !self.suggestions.contains(&s) {
                self.suggestions.push(s);
            }
        }
        for name in page.unresponsive {
            if self.unresponsive.len() < MAX_UNRESPONSIVE && !self.unresponsive.contains(&name) {
                self.unresponsive.push(name);
            }
        }
    }
}

fn header_pair(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), ConfigError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| ConfigError::InvalidHeaderName(name.to_owned()))?;
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| ConfigError::Http("auth header value is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok((name, value))
}

#[async_trait]
impl SearchBackend for SearXngBackend {
    fn id(&self) -> BackendId {
        BackendId::SearXng
    }

    async fn search(
        &self,
        req: &SearchRequest,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse, BackendError> {
        tokio::select! {
            _ = cancel.cancelled() => Err(BackendError::Cancelled),
            result = self.run(req) => result,
        }
    }

    async fn probe(&self) -> Result<ProbeReport, BackendError> {
        let req = SearchRequest {
            query: "agentkit probe".into(),
            max_results: 1,
            freshness: None,
            include_domains: vec![],
            language: None,
        };
        let started = Instant::now();
        let resp = self.search(&req, &CancellationToken::new()).await?;
        Ok(ProbeReport {
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            result_count: resp.hits.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_preserves_base_path() {
        for (base, want) in [
            ("https://h/", "https://h/search"),
            ("https://h", "https://h/search"),
            ("https://h/searx", "https://h/searx/search"),
            ("https://h/searx/", "https://h/searx/search"),
        ] {
            let cfg = SearXngConfig::new(Url::parse(base).unwrap());
            let b = SearXngBackend::new(cfg, Duration::from_secs(1), 1024).unwrap();
            assert_eq!(b.endpoint.as_str(), want);
        }
    }

    #[test]
    fn bad_header_name_rejected() {
        let mut cfg = SearXngConfig::new(Url::parse("https://h/").unwrap());
        cfg.auth = SearXngAuth::Header {
            name: "bad name".into(),
            value: zeroize::Zeroizing::new("v".into()),
        };
        assert!(matches!(
            SearXngBackend::new(cfg, Duration::from_secs(1), 1024),
            Err(ConfigError::InvalidHeaderName(_))
        ));
    }
}
