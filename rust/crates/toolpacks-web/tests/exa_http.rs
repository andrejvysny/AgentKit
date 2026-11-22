//! Exa backend against local 127.0.0.1 stubs.

mod common;

use std::time::Duration;

use agentkit_toolpacks_web::*;
use common::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

const RESULTS: &str = include_str!("fixtures/exa/results.json");
const EMPTY: &str = include_str!("fixtures/exa/empty.json");

fn backend(base: &str) -> ExaBackend {
    let mut cfg = ExaConfig::new("exa-key-456");
    cfg.endpoint = Url::parse(&format!("{base}/search")).unwrap();
    ExaBackend::new(cfg, Duration::from_secs(5), 2 * 1024 * 1024).unwrap()
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        max_results: 6,
        freshness: None,
        include_domains: vec![],
        language: Some("en".into()),
    }
}

async fn search(b: &ExaBackend, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
    b.search(req, &CancellationToken::new()).await
}

fn sent(stub: &Stub) -> Value {
    serde_json::from_str(body(&stub.requests()[0])).unwrap()
}

#[tokio::test]
async fn request_shape_without_optionals() {
    let stub = serve(vec![ok(RESULTS)]);
    search(&backend(&stub.base), &request("tokio"))
        .await
        .unwrap();
    let raw = &stub.requests()[0];
    assert_eq!(request_line(raw), "POST /search HTTP/1.1");
    assert_eq!(header(raw, "x-api-key").as_deref(), Some("exa-key-456"));
    assert_eq!(
        header(raw, "content-type").as_deref(),
        Some("application/json")
    );
    let json = sent(&stub);
    assert_eq!(json["query"], "tokio");
    assert_eq!(json["numResults"], 8);
    assert_eq!(json["type"], "auto");
    assert_eq!(json["contents"]["highlights"]["numSentences"], 3);
    assert_eq!(json["contents"]["highlights"]["highlightsPerUrl"], 2);
    assert!(json.get("includeDomains").is_none());
    assert!(json.get("startPublishedDate").is_none());
    assert!(!body(raw).contains("exa-key"));
}

#[tokio::test]
async fn freshness_domains_and_type() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut cfg = ExaConfig::new("k");
    cfg.endpoint = Url::parse(&format!("{}/search", stub.base)).unwrap();
    cfg.search_type = ExaSearchType::Neural;
    let b = ExaBackend::new(cfg, Duration::from_secs(5), 1 << 20).unwrap();
    let mut req = request("tokio");
    req.max_results = 10;
    req.freshness = Some(Freshness::Day);
    req.include_domains = vec!["WWW.Tokio.rs".into()];
    search(&b, &req).await.unwrap();
    let json = sent(&stub);
    assert_eq!(json["numResults"], 12);
    assert_eq!(json["type"], "neural");
    assert_eq!(json["includeDomains"], serde_json::json!(["tokio.rs"]));
    let start = json["startPublishedDate"].as_str().unwrap();
    assert!(start.ends_with('Z') && start.len() == 20, "{start}");
    assert!(start.as_bytes()[10] == b'T');
}

#[tokio::test]
async fn parses_highlights_titles_and_dedupes() {
    let stub = serve(vec![ok(RESULTS)]);
    let resp = search(&backend(&stub.base), &request("tokio"))
        .await
        .unwrap();
    let urls: Vec<String> = resp.hits.iter().map(|h| h.url.to_string()).collect();
    assert_eq!(
        urls,
        [
            "https://tokio.rs/",
            "https://www.example.com/untitled",
            "https://sub.docs.rs/x",
            "https://notdocs.rs/y",
        ]
    );
    let first = &resp.hits[0];
    assert_eq!(
        first.snippet,
        "Tokio is an event-driven, non-blocking I/O platform."
    );
    assert_eq!(
        first.extra,
        [
            "It provides a multi-threaded scheduler.",
            "Third highlight kept as extra."
        ]
    );
    assert_eq!(first.published.as_deref(), Some("2026-01-15T00:00:00.000Z"));
    assert_eq!(first.engine, "exa");
    let second = &resp.hits[1];
    assert_eq!(second.title, "example.com");
    assert_eq!(second.snippet, "Fallback text from the page body.");
    assert!(second.extra.is_empty());
    assert_eq!(resp.backend, BackendId::Exa);
}

#[tokio::test]
async fn domain_post_filter() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio");
    req.include_domains = vec!["docs.rs".into()];
    let resp = search(&backend(&stub.base), &req).await.unwrap();
    let urls: Vec<String> = resp.hits.iter().map(|h| h.url.to_string()).collect();
    assert_eq!(urls, ["https://sub.docs.rs/x"]);
}

#[tokio::test]
async fn status_mapping() {
    let cases: Vec<(Resp, BackendError)> = vec![
        (status("401 Unauthorized", ""), BackendError::Unauthorized),
        (status("403 Forbidden", ""), BackendError::Unauthorized),
        (
            status("402 Payment Required", ""),
            BackendError::QuotaExhausted,
        ),
        (
            status("429 Too Many Requests", ""),
            BackendError::RateLimited { retry_after: None },
        ),
        (
            status("400 Bad Request", "echoed secret"),
            BackendError::Rejected("exa rejected the request (HTTP 400)".into()),
        ),
        (
            status("502 Bad Gateway", ""),
            BackendError::Unavailable(502),
        ),
    ];
    for (resp, want) in cases {
        let stub = serve(vec![resp]);
        let got = search(&backend(&stub.base), &request("x"))
            .await
            .unwrap_err();
        assert_eq!(got, want);
    }
}

#[tokio::test]
async fn malformed_empty_and_probe() {
    let stub = serve(vec![ok("{\"nope\":1}")]);
    let err = search(&backend(&stub.base), &request("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::BadResponse(_)));

    let stub = serve(vec![ok(EMPTY)]);
    let resp = search(&backend(&stub.base), &request("x")).await.unwrap();
    assert!(resp.hits.is_empty());

    let stub = serve(vec![ok(RESULTS)]);
    let report = backend(&stub.base).probe().await.unwrap();
    assert_eq!(report.result_count, 1);
    assert_eq!(sent(&stub)["numResults"], 1);
}

#[test]
fn config_rejects_bad_endpoint_and_empty_key() {
    let mut cfg = ExaConfig::new("k");
    cfg.endpoint = Url::parse("http://example.com/search").unwrap();
    assert!(matches!(
        ExaBackend::new(cfg, Duration::from_secs(1), 1024),
        Err(ConfigError::InvalidBaseUrl(_))
    ));
    assert!(matches!(
        ExaBackend::new(ExaConfig::new(" "), Duration::from_secs(1), 1024),
        Err(ConfigError::MissingApiKey("exa"))
    ));
}
