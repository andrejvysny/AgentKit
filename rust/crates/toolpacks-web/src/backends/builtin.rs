//! Built-in keyless backend: Exa's public MCP endpoint (`web_search_exa`
//! over JSON-RPC). Needs no credentials, so it can be the default when no
//! backend is configured. Freshness and language are not supported by the
//! endpoint and are ignored.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::backend::*;
use crate::config::{BuiltinConfig, ConfigError};
use crate::normalize::{SNIPPET_MAX, TITLE_MAX, canonical_url, clean_text, domain_of};

use super::http_common::{build_client, check_status, map_transport, read_capped};
use super::merge::{HitSet, normalize_domains, with_site_operator};

const MAX_RESULTS: usize = 20;
const PUBLISHED_MAX: usize = 40;
const OBJECTIVE_MAX: usize = 1024;
const BLOCK_SEPARATOR: &str = "\n\n---\n\n";

pub struct BuiltinBackend {
    client: Client,
    endpoint: Url,
    max_body_bytes: usize,
}

impl BuiltinBackend {
    pub fn new(
        cfg: BuiltinConfig,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            client: build_client(request_timeout)?,
            endpoint: cfg.endpoint,
            max_body_bytes,
        })
    }

    fn body(query: &str, num: usize) -> Value {
        let objective: String = query.chars().take(OBJECTIVE_MAX).collect();
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {"query": query, "objective": objective, "numResults": num},
            },
        })
    }

    async fn fetch(&self, query: &str, num: usize) -> Result<Vec<SearchHit>, BackendError> {
        let payload = serde_json::to_vec(&Self::body(query, num))
            .map_err(|_| BackendError::InvalidRequest("request not serialisable".into()))?;
        let resp = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(payload)
            .send()
            .await
            .map_err(map_transport)?;
        check_status(&resp, |code| match code {
            401 | 403 => Some(BackendError::Unauthorized),
            _ => None,
        })?;
        parse_hits(&read_capped(resp, self.max_body_bytes).await?)
    }

    async fn run(&self, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let domains = normalize_domains(&req.include_domains)?;
        let max = usize::from(req.max_results.max(1));
        let query = with_site_operator(&req.query, &domains);
        let mut set = HitSet::default();
        set.absorb(
            self.fetch(&query, (max + 2).min(MAX_RESULTS)).await?,
            &domains,
        );
        set.hits.truncate(max);
        Ok(SearchResponse {
            hits: set.hits,
            backend: BackendId::Builtin,
            suggestions: vec![],
            answer: None,
        })
    }
}

/// The JSON-RPC message, whether the server answered plain JSON or SSE.
fn rpc_message(body: &[u8]) -> Result<Value, BackendError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| BackendError::BadResponse("response is not UTF-8".into()))?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|_| BackendError::BadResponse("invalid JSON".into()));
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .ok_or_else(|| BackendError::BadResponse("no message in response".into()))
}

fn field<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    block
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .map(str::trim)
}

fn build_hit(block: &str) -> Option<SearchHit> {
    let url = canonical_url(field(block, "URL:")?)?;
    let title = field(block, "Title:")
        .map(|t| clean_text(t, TITLE_MAX))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| domain_of(&url));
    let published = field(block, "Published:")
        .filter(|p| !p.eq_ignore_ascii_case("n/a"))
        .map(|p| clean_text(p, PUBLISHED_MAX))
        .filter(|p| !p.is_empty());
    let highlights = block
        .split_once("Highlights:")
        .map_or("", |(_, rest)| rest.trim());
    Some(SearchHit {
        title,
        snippet: clean_text(highlights, SNIPPET_MAX),
        published,
        extra: vec![],
        engine: "builtin".into(),
        url,
    })
}

/// Result blocks are `---` separated; a highlight that itself contains a
/// separator yields a piece without a `Title:` head, which is folded back
/// into the previous block.
fn split_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for piece in text.split(BLOCK_SEPARATOR) {
        match blocks.last_mut() {
            Some(last) if !piece.trim_start().starts_with("Title:") => {
                last.push_str(BLOCK_SEPARATOR);
                last.push_str(piece);
            }
            _ => blocks.push(piece.to_owned()),
        }
    }
    blocks
}

fn parse_hits(body: &[u8]) -> Result<Vec<SearchHit>, BackendError> {
    let message = rpc_message(body)?;
    if message.get("error").is_some() {
        return Err(BackendError::BadResponse("rpc error".into()));
    }
    let result = message
        .get("result")
        .ok_or_else(|| BackendError::BadResponse("missing result".into()))?;
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(BLOCK_SEPARATOR)
        })
        .ok_or_else(|| BackendError::BadResponse("missing content".into()))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(BackendError::Unavailable(502));
    }
    Ok(split_blocks(&text)
        .iter()
        .filter_map(|block| build_hit(block))
        .collect())
}

#[async_trait]
impl SearchBackend for BuiltinBackend {
    fn id(&self) -> BackendId {
        BackendId::Builtin
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
        let started = Instant::now();
        let hits = self.fetch("agentkit probe", 1).await?;
        Ok(ProbeReport {
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            result_count: hits.len().min(1),
        })
    }
}
