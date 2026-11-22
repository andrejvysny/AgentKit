//! Built-in keyless backend against local 127.0.0.1 stubs.

mod common;

use std::time::Duration;

use agentkit_toolpacks_web::*;
use common::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

const RESULTS: &str = include_str!("fixtures/builtin/results.sse");
const EMPTY: &str = include_str!("fixtures/builtin/empty.sse");

fn backend(base: &str) -> BuiltinBackend {
    let mut cfg = BuiltinConfig::new();
    cfg.endpoint = Url::parse(&format!("{base}/mcp")).unwrap();
    BuiltinBackend::new(cfg, Duration::from_secs(5), 2 * 1024 * 1024).unwrap()
}

fn request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        max_results: 6,
        freshness: None,
        include_domains: vec![],
        language: None,
    }
}

async fn search(b: &BuiltinBackend, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
    b.search(req, &CancellationToken::new()).await
}

#[tokio::test]
async fn sends_keyless_jsonrpc_call() {
    let stub = serve(vec![ok(RESULTS)]);
    search(&backend(&stub.base), &request("tokio"))
        .await
        .unwrap();
    let raw = &stub.requests()[0];
    assert_eq!(request_line(raw), "POST /mcp HTTP/1.1");
    assert!(header(raw, "x-api-key").is_none());
    assert!(header(raw, "accept").unwrap().contains("text/event-stream"));
    let json: Value = serde_json::from_str(body(raw)).unwrap();
    assert_eq!(json["method"], "tools/call");
    assert_eq!(json["params"]["name"], "web_search_exa");
    assert_eq!(json["params"]["arguments"]["query"], "tokio");
    assert_eq!(json["params"]["arguments"]["objective"], "tokio");
    assert_eq!(json["params"]["arguments"]["numResults"], 8);
}

#[tokio::test]
async fn parses_sse_blocks_and_folds_separator_inside_highlights() {
    let stub = serve(vec![ok(RESULTS)]);
    let resp = search(&backend(&stub.base), &request("tokio"))
        .await
        .unwrap();
    assert_eq!(resp.backend, BackendId::Builtin);
    assert_eq!(resp.hits.len(), 2);
    assert_eq!(resp.hits[0].title, "tokio::runtime - Rust");
    assert!(resp.hits[0].snippet.contains("still the same highlight"));
    assert_eq!(resp.hits[0].published, None);
    assert_eq!(resp.hits[0].engine, "builtin");
    assert_eq!(resp.hits[1].published.as_deref(), Some("2024-05-01"));
}

#[tokio::test]
async fn single_domain_becomes_site_operator_and_filters() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio");
    req.include_domains = vec!["tokio.rs".into()];
    let resp = search(&backend(&stub.base), &req).await.unwrap();
    let json: Value = serde_json::from_str(body(&stub.requests()[0])).unwrap();
    assert_eq!(json["params"]["arguments"]["query"], "tokio site:tokio.rs");
    assert_eq!(resp.hits.len(), 1);
    assert_eq!(resp.hits[0].url.host_str(), Some("tokio.rs"));
}

#[tokio::test]
async fn empty_text_is_no_hits() {
    let stub = serve(vec![ok(EMPTY)]);
    let resp = search(&backend(&stub.base), &request("x")).await.unwrap();
    assert!(resp.hits.is_empty());
}

#[tokio::test]
async fn plain_json_reply_is_accepted() {
    let stub = serve(vec![ok(
        r#"{"result":{"content":[{"type":"text","text":"Title: A\nURL: https://a.example/\nHighlights:\nhi"}]}}"#,
    )]);
    let resp = search(&backend(&stub.base), &request("a")).await.unwrap();
    assert_eq!(resp.hits.len(), 1);
}

#[tokio::test]
async fn error_mapping() {
    let stub = serve(vec![
        status("429 Too Many Requests", ""),
        status("503 Service Unavailable", ""),
        ok(r#"{"error":{"code":-1,"message":"boom"}}"#),
        ok("garbage"),
    ]);
    let b = backend(&stub.base);
    let r = request("q");
    assert!(matches!(
        search(&b, &r).await,
        Err(BackendError::RateLimited { .. })
    ));
    assert_eq!(
        search(&b, &r).await.unwrap_err(),
        BackendError::Unavailable(503)
    );
    assert!(matches!(
        search(&b, &r).await,
        Err(BackendError::BadResponse(_))
    ));
    assert!(matches!(
        search(&b, &r).await,
        Err(BackendError::BadResponse(_))
    ));
}

#[tokio::test]
async fn probe_reports_one_result() {
    let stub = serve(vec![ok(RESULTS)]);
    let report = backend(&stub.base).probe().await.unwrap();
    assert_eq!(report.result_count, 1);
}
