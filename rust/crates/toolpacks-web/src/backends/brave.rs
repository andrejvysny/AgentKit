//! Brave Search API backend (`GET {endpoint}?q=...`, token header auth).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::backend::*;
use crate::config::{BraveConfig, BraveSafesearch, ConfigError};
use crate::normalize::{SNIPPET_MAX, TITLE_MAX, canonical_url, clean_text, strip_tags};

use super::http_common::{
    build_client, check_status, keyed_api_status, map_transport, read_capped,
};
use super::merge::{HitSet, normalize_domains, with_site_operator};

const MAX_COUNT: usize = 20;
const PUBLISHED_MAX: usize = 40;
const EXTRA_MAX: usize = 2;

pub struct BraveBackend {
    client: Client,
    endpoint: Url,
    token: HeaderValue,
    country: Option<String>,
    safesearch: BraveSafesearch,
    max_body_bytes: usize,
}

impl BraveBackend {
    pub fn new(
        cfg: BraveConfig,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let mut token = HeaderValue::from_str(cfg.api_key.trim())
            .map_err(|_| ConfigError::Http("brave api key is not a valid header value".into()))?;
        token.set_sensitive(true);
        Ok(Self {
            client: build_client(request_timeout)?,
            endpoint: cfg.endpoint,
            token,
            country: cfg.country,
            safesearch: cfg.safesearch,
            max_body_bytes,
        })
    }

    fn request_url(&self, req: &SearchRequest, domains: &[String], count: usize) -> Url {
        let mut url = self.endpoint.clone();
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", &with_site_operator(&req.query, domains))
                .append_pair("count", &count.to_string())
                .append_pair("offset", "0")
                .append_pair("extra_snippets", "true")
                .append_pair(
                    "safesearch",
                    match self.safesearch {
                        BraveSafesearch::Off => "off",
                        BraveSafesearch::Moderate => "moderate",
                        BraveSafesearch::Strict => "strict",
                    },
                );
            if let Some(lang) = &req.language {
                pairs.append_pair("search_lang", lang);
            }
            if let Some(country) = &self.country {
                pairs.append_pair("country", country);
            }
            if let Some(f) = req.freshness {
                let code = match f {
                    Freshness::Day => "pd",
                    Freshness::Week => "pw",
                    Freshness::Month => "pm",
                    Freshness::Year => "py",
                };
                pairs.append_pair("freshness", code);
            }
        }
        url
    }

    async fn fetch(&self, url: Url) -> Result<Vec<SearchHit>, BackendError> {
        let resp = self
            .client
            .get(url)
            .header("X-Subscription-Token", self.token.clone())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(map_transport)?;
        check_status(&resp, keyed_api_status("brave", &[400, 422]))?;
        parse_hits(&read_capped(resp, self.max_body_bytes).await?)
    }

    async fn run(&self, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let domains = normalize_domains(&req.include_domains)?;
        let max = usize::from(req.max_results.max(1));
        let url = self.request_url(req, &domains, (max + 4).min(MAX_COUNT));
        let mut set = HitSet::default();
        set.absorb(self.fetch(url).await?, &domains);
        set.hits.truncate(max);
        Ok(SearchResponse {
            hits: set.hits,
            backend: BackendId::Brave,
            suggestions: vec![],
            answer: None,
        })
    }
}

fn text(item: &Value, key: &str, max: usize) -> String {
    let raw = item.get(key).and_then(Value::as_str).unwrap_or("");
    clean_text(&strip_tags(raw), max)
}

fn build_hit(item: &Value) -> Option<SearchHit> {
    let url = canonical_url(item.get("url")?.as_str()?)?;
    let extra = item
        .get("extra_snippets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .take(EXTRA_MAX)
                .map(|s| clean_text(&strip_tags(s), SNIPPET_MAX))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let published = ["page_age", "age"]
        .iter()
        .find_map(|k| item.get(*k).and_then(Value::as_str))
        .map(|p| clean_text(p, PUBLISHED_MAX))
        .filter(|p| !p.is_empty());
    Some(SearchHit {
        title: text(item, "title", TITLE_MAX),
        snippet: text(item, "description", SNIPPET_MAX),
        published,
        extra,
        engine: "brave".into(),
        url,
    })
}

fn parse_hits(body: &[u8]) -> Result<Vec<SearchHit>, BackendError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| BackendError::BadResponse("invalid JSON".into()))?;
    let Some(results) = value
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(Value::as_array)
    else {
        return Ok(vec![]);
    };
    Ok(results.iter().filter_map(build_hit).collect())
}

#[async_trait]
impl SearchBackend for BraveBackend {
    fn id(&self) -> BackendId {
        BackendId::Brave
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
        let hits = self.fetch(self.request_url(&req, &[], 1)).await?;
        Ok(ProbeReport {
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            result_count: hits.len().min(1),
        })
    }
}
