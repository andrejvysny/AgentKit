//! Exa search API backend (`POST {endpoint}` with a JSON body, `x-api-key`).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use reqwest::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::backend::*;
use crate::config::{ConfigError, ExaConfig, ExaSearchType};
use crate::normalize::{SNIPPET_MAX, TITLE_MAX, canonical_url, clean_text, domain_of};

use super::http_common::{
    build_client, check_status, keyed_api_status, map_transport, read_capped,
};
use super::merge::{HitSet, normalize_domains};

const MAX_RESULTS: usize = 20;
const PUBLISHED_MAX: usize = 40;

pub struct ExaBackend {
    client: Client,
    endpoint: Url,
    key: HeaderValue,
    search_type: ExaSearchType,
    max_body_bytes: usize,
}

impl ExaBackend {
    pub fn new(
        cfg: ExaConfig,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let mut key = HeaderValue::from_str(cfg.api_key.trim())
            .map_err(|_| ConfigError::Http("exa api key is not a valid header value".into()))?;
        key.set_sensitive(true);
        Ok(Self {
            client: build_client(request_timeout)?,
            endpoint: cfg.endpoint,
            key,
            search_type: cfg.search_type,
            max_body_bytes,
        })
    }

    fn body(&self, req: &SearchRequest, domains: &[String], num: usize) -> Value {
        let mut body = json!({
            "query": req.query,
            "numResults": num,
            "type": self.search_type.as_str(),
            "contents": {"highlights": {"numSentences": 3, "highlightsPerUrl": 2}},
        });
        if !domains.is_empty() {
            body["includeDomains"] = json!(domains);
        }
        if let Some(f) = req.freshness {
            let days = match f {
                Freshness::Day => 1,
                Freshness::Week => 7,
                Freshness::Month => 30,
                Freshness::Year => 365,
            };
            let since = Utc::now() - chrono::Duration::days(days);
            body["startPublishedDate"] = json!(since.to_rfc3339_opts(SecondsFormat::Secs, true));
        }
        body
    }

    async fn fetch(&self, body: &Value) -> Result<Vec<SearchHit>, BackendError> {
        let payload = serde_json::to_vec(body)
            .map_err(|_| BackendError::InvalidRequest("request not serialisable".into()))?;
        let resp = self
            .client
            .post(self.endpoint.clone())
            .header("x-api-key", self.key.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(map_transport)?;
        check_status(&resp, keyed_api_status("exa", &[400]))?;
        parse_hits(&read_capped(resp, self.max_body_bytes).await?)
    }

    async fn run(&self, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let domains = normalize_domains(&req.include_domains)?;
        let max = usize::from(req.max_results.max(1));
        let body = self.body(req, &domains, (max + 2).min(MAX_RESULTS));
        let mut set = HitSet::default();
        set.absorb(self.fetch(&body).await?, &domains);
        set.hits.truncate(max);
        Ok(SearchResponse {
            hits: set.hits,
            backend: BackendId::Exa,
            suggestions: vec![],
            answer: None,
        })
    }
}

fn highlights(item: &Value) -> Vec<&str> {
    item.get("highlights")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn build_hit(item: &Value) -> Option<SearchHit> {
    let url = canonical_url(item.get("url")?.as_str()?)?;
    let marks = highlights(item);
    let snippet_src = marks
        .first()
        .copied()
        .or_else(|| item.get("text").and_then(Value::as_str))
        .unwrap_or("");
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|t| clean_text(t, TITLE_MAX))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| domain_of(&url));
    let published = item
        .get("publishedDate")
        .and_then(Value::as_str)
        .map(|p| clean_text(p, PUBLISHED_MAX))
        .filter(|p| !p.is_empty());
    Some(SearchHit {
        title,
        snippet: clean_text(snippet_src, SNIPPET_MAX),
        published,
        extra: marks
            .iter()
            .skip(1)
            .take(2)
            .map(|s| clean_text(s, SNIPPET_MAX))
            .filter(|s| !s.is_empty())
            .collect(),
        engine: "exa".into(),
        url,
    })
}

fn parse_hits(body: &[u8]) -> Result<Vec<SearchHit>, BackendError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| BackendError::BadResponse("invalid JSON".into()))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError::BadResponse("missing results".into()))?;
    Ok(results.iter().filter_map(build_hit).collect())
}

#[async_trait]
impl SearchBackend for ExaBackend {
    fn id(&self) -> BackendId {
        BackendId::Exa
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
        let hits = self.fetch(&self.body(&req, &[], 1)).await?;
        Ok(ProbeReport {
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            result_count: hits.len().min(1),
        })
    }
}
