//! Brave backend against local 127.0.0.1 stubs.

mod common;

use std::time::Duration;

use agentkit_toolpacks_web::*;
use common::*;
use tokio_util::sync::CancellationToken;
use url::Url;

const RESULTS: &str = include_str!("fixtures/brave/results.json");
const NO_WEB: &str = include_str!("fixtures/brave/no_web.json");

fn backend(base: &str) -> BraveBackend {
    let mut cfg = BraveConfig::new("brave-key-123");
    cfg.endpoint = Url::parse(&format!("{base}/res/v1/web/search")).unwrap();
    BraveBackend::new(cfg, Duration::from_secs(5), 2 * 1024 * 1024).unwrap()
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

async fn search(b: &BraveBackend, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
    b.search(req, &CancellationToken::new()).await
}

fn query_of(raw: &str) -> Vec<(String, String)> {
    let target = request_line(raw).split(' ').nth(1).unwrap();
    Url::parse(&format!("http://h{target}"))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn param(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[tokio::test]
async fn request_shape() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio runtime");
    req.language = Some("en".into());
    req.freshness = Some(Freshness::Week);
    search(&backend(&stub.base), &req).await.unwrap();
    let raw = &stub.requests()[0];
    assert!(
        request_line(raw).starts_with("GET /res/v1/web/search?"),
        "{raw}"
    );
    assert_eq!(
        header(raw, "x-subscription-token").as_deref(),
        Some("brave-key-123")
    );
    assert_eq!(header(raw, "accept").as_deref(), Some("application/json"));
    assert_eq!(
        header(raw, "user-agent").as_deref(),
        Some("AgentKit-WebSearch/0.1")
    );
    let q = query_of(raw);
    assert_eq!(param(&q, "q").as_deref(), Some("tokio runtime"));
    assert_eq!(param(&q, "count").as_deref(), Some("10"));
    assert_eq!(param(&q, "offset").as_deref(), Some("0"));
    assert_eq!(param(&q, "search_lang").as_deref(), Some("en"));
    assert_eq!(param(&q, "safesearch").as_deref(), Some("moderate"));
    assert_eq!(param(&q, "freshness").as_deref(), Some("pw"));
    assert_eq!(param(&q, "extra_snippets").as_deref(), Some("true"));
    assert_eq!(param(&q, "country"), None);
}

#[tokio::test]
async fn count_is_capped_and_country_safesearch_apply() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut cfg = BraveConfig::new("k");
    cfg.endpoint = Url::parse(&format!("{}/s", stub.base)).unwrap();
    cfg.country = Some("SK".into());
    cfg.safesearch = BraveSafesearch::Strict;
    let b = BraveBackend::new(cfg, Duration::from_secs(5), 1 << 20).unwrap();
    let mut req = request("x");
    req.max_results = 10;
    search(&b, &req).await.unwrap();
    let q = query_of(&stub.requests()[0]);
    assert_eq!(param(&q, "count").as_deref(), Some("14"));
    assert_eq!(param(&q, "country").as_deref(), Some("SK"));
    assert_eq!(param(&q, "safesearch").as_deref(), Some("strict"));
}

#[tokio::test]
async fn parses_strips_tags_and_dedupes() {
    let stub = serve(vec![ok(RESULTS)]);
    let resp = search(&backend(&stub.base), &request("tokio"))
        .await
        .unwrap();
    let urls: Vec<String> = resp.hits.iter().map(|h| h.url.to_string()).collect();
    assert_eq!(
        urls,
        [
            "https://docs.rs/tokio/latest/tokio",
            "https://tokio.rs/tokio/tutorial",
            "https://sub.docs.rs/x",
            "https://notdocs.rs/y",
        ]
    );
    let first = &resp.hits[0];
    assert_eq!(first.title, "Tokio - Rust async runtime");
    assert_eq!(
        first.snippet,
        "A runtime for writing reliable & fast network apps \"fast\" it's <b>ok</b>."
    );
    assert_eq!(first.extra, ["First extra & more", "Second extra"]);
    assert_eq!(first.published.as_deref(), Some("2026-02-03T10:00:00"));
    assert_eq!(first.engine, "brave");
    assert_eq!(resp.backend, BackendId::Brave);
}

#[tokio::test]
async fn missing_web_is_empty_not_error() {
    let stub = serve(vec![ok(NO_WEB)]);
    let resp = search(&backend(&stub.base), &request("zzz")).await.unwrap();
    assert!(resp.hits.is_empty());
}

#[tokio::test]
async fn single_domain_adds_site_and_filters() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio");
    req.include_domains = vec!["docs.rs".into()];
    let resp = search(&backend(&stub.base), &req).await.unwrap();
    let q = query_of(&stub.requests()[0]);
    assert_eq!(param(&q, "q").as_deref(), Some("tokio site:docs.rs"));
    let urls: Vec<String> = resp.hits.iter().map(|h| h.url.to_string()).collect();
    assert_eq!(
        urls,
        [
            "https://docs.rs/tokio/latest/tokio",
            "https://sub.docs.rs/x"
        ]
    );

    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio");
    req.include_domains = vec!["docs.rs".into(), "tokio.rs".into()];
    let resp = search(&backend(&stub.base), &req).await.unwrap();
    let q = query_of(&stub.requests()[0]);
    assert_eq!(param(&q, "q").as_deref(), Some("tokio"));
    assert_eq!(resp.hits.len(), 3);

    let mut req = request("tokio");
    req.include_domains = vec!["bad host".into()];
    let err = search(&backend(&stub.base), &req).await.unwrap_err();
    assert!(matches!(err, BackendError::InvalidRequest(_)));
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
            Resp {
                headers: vec![("Retry-After", "7")],
                ..status("429 Too Many Requests", "")
            },
            BackendError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (
            status("422 Unprocessable Entity", "secret detail"),
            BackendError::Rejected("brave rejected the request (HTTP 422)".into()),
        ),
        (
            status("400 Bad Request", "secret detail"),
            BackendError::Rejected("brave rejected the request (HTTP 400)".into()),
        ),
        (
            status("404 Not Found", ""),
            BackendError::BadResponse("HTTP 404".into()),
        ),
        (
            status("503 Service Unavailable", ""),
            BackendError::Unavailable(503),
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
async fn malformed_json_and_oversize() {
    let stub = serve(vec![ok("{nope")]);
    let err = search(&backend(&stub.base), &request("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::BadResponse(_)));

    let stub = serve(vec![ok(RESULTS)]);
    let mut cfg = BraveConfig::new("k");
    cfg.endpoint = Url::parse(&format!("{}/s", stub.base)).unwrap();
    let b = BraveBackend::new(cfg, Duration::from_secs(5), 128).unwrap();
    let err = search(&b, &request("x")).await.unwrap_err();
    assert_eq!(err, BackendError::BadResponse("response too large".into()));
}

#[tokio::test]
async fn probe_asks_for_one_result() {
    let stub = serve(vec![ok(RESULTS)]);
    let report = backend(&stub.base).probe().await.unwrap();
    assert_eq!(report.result_count, 1);
    let q = query_of(&stub.requests()[0]);
    assert_eq!(param(&q, "count").as_deref(), Some("1"));
}

#[test]
fn config_rejects_bad_endpoint_and_empty_key() {
    let mut cfg = BraveConfig::new("k");
    cfg.endpoint = Url::parse("http://example.com/s").unwrap();
    assert!(matches!(
        BraveBackend::new(cfg, Duration::from_secs(1), 1024),
        Err(ConfigError::InvalidBaseUrl(_))
    ));
    assert!(matches!(
        BraveBackend::new(BraveConfig::new(""), Duration::from_secs(1), 1024),
        Err(ConfigError::MissingApiKey("brave"))
    ));
}
