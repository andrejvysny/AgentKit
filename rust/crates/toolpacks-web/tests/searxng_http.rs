//! SearXNG backend and `web_search` tool against local 127.0.0.1 stubs.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_core::{Tool, ToolContext, ToolLimits};
use agentkit_governance::{ContributedTool, ToolContributionContext};
use agentkit_toolpacks_web::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::Zeroizing;

const RESULTS: &str = include_str!("fixtures/searxng/results.json");
const PAGE2: &str = include_str!("fixtures/searxng/page2.json");
const ENGINES_DOWN: &str = include_str!("fixtures/searxng/engines_unavailable.json");
const EMPTY_ONE: &str =
    r#"{"results":[{"url":"https://one.example/a","title":"one","content":"c"}]}"#;
const EMPTY: &str = include_str!("fixtures/searxng/empty.json");

struct Resp {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
    delay: Duration,
}

fn ok(body: &str) -> Resp {
    status("200 OK", body)
}

fn status(status: &'static str, body: &str) -> Resp {
    Resp {
        status,
        headers: vec![],
        body: body.to_owned(),
        delay: Duration::ZERO,
    }
}

struct Stub {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Serves `responses` to sequential connections; the thread is detached.
fn serve(responses: Vec<Resp>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    std::thread::spawn(move || {
        for resp in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw).into_owned());
            std::thread::sleep(resp.delay);
            let mut head = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                resp.status,
                resp.body.len()
            );
            for (k, v) in resp.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(resp.body.as_bytes());
        }
    });
    Stub { base, requests }
}

fn config(url: &str) -> SearXngConfig {
    SearXngConfig::new(Url::parse(url).unwrap())
}

fn backend(cfg: SearXngConfig) -> SearXngBackend {
    SearXngBackend::new(cfg, Duration::from_secs(5), 2 * 1024 * 1024).unwrap()
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

async fn search(b: &SearXngBackend, req: &SearchRequest) -> Result<SearchResponse, BackendError> {
    b.search(req, &CancellationToken::new()).await
}

fn urls(resp: &SearchResponse) -> Vec<String> {
    resp.hits.iter().map(|h| h.url.to_string()).collect()
}

#[tokio::test]
async fn success_dedupes_skips_non_http_and_parses_extras() {
    let stub = serve(vec![ok(RESULTS)]);
    let resp = search(&backend(config(&stub.base)), &request("tokio"))
        .await
        .unwrap();
    assert_eq!(
        urls(&resp),
        [
            "https://docs.rs/tokio/latest/tokio",
            "https://www.example.com/a",
            "https://blog.rust-lang.org/2026/01/01/release",
            "https://doc.rust-lang.org/std",
            "https://sub.docs.rs/x",
            "https://notdocs.rs/y",
        ]
    );
    assert_eq!(resp.hits[0].engine, "searxng:google");
    assert_eq!(resp.hits[0].extra, ["google", "bing"]);
    assert_eq!(
        resp.hits[0].snippet,
        "A runtime for writing reliable asynchronous applications."
    );
    assert_eq!(
        resp.hits[2].published.as_deref(),
        Some("2026-01-01T00:00:00")
    );
    assert_eq!(resp.answer.as_deref(), Some("tokio is an async runtime"));
    assert_eq!(resp.suggestions.len(), 3);
    assert_eq!(stub.requests().len(), 1, "6 hits reached: no page 2");
}

#[tokio::test]
async fn base_path_and_query_params() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut cfg = config(&format!("{}/searx/", stub.base));
    cfg.language = Some("de".into());
    let mut req = request("rust 1.97");
    req.freshness = Some(Freshness::Week);
    req.language = Some("sk".into());
    search(&backend(cfg), &req).await.unwrap();
    let line = stub.requests()[0].lines().next().unwrap().to_owned();
    assert!(line.starts_with("GET /searx/search?"), "{line}");
    for want in [
        "q=rust+1.97",
        "format=json",
        "pageno=1",
        "categories=general",
        "safesearch=1",
        "language=sk",
        "time_range=week",
    ] {
        assert!(line.contains(want), "{want} in {line}");
    }
    assert!(!line.contains("language=de"));
}

#[tokio::test]
async fn basic_and_header_auth() {
    let stub = serve(vec![ok(EMPTY)]);
    let mut cfg = config(&stub.base);
    cfg.auth = SearXngAuth::Basic {
        username: "user".into(),
        password: Zeroizing::new("pass".into()),
    };
    search(&backend(cfg), &request("x")).await.unwrap();
    let raw = stub.requests()[0].to_ascii_lowercase();
    assert!(raw.contains("authorization: basic dxnlcjpwyxnz"), "{raw}");

    let stub = serve(vec![ok(EMPTY)]);
    let mut cfg = config(&stub.base);
    cfg.auth = SearXngAuth::Header {
        name: "X-Api-Key".into(),
        value: Zeroizing::new("k-123".into()),
    };
    search(&backend(cfg), &request("x")).await.unwrap();
    let raw = stub.requests()[0].to_ascii_lowercase();
    assert!(raw.contains("x-api-key: k-123"), "{raw}");
    assert!(raw.contains("user-agent: agentkit-websearch/0.1"));
}

#[tokio::test]
async fn single_domain_adds_site_and_filters_by_host_then_fetches_page_two() {
    let stub = serve(vec![ok(RESULTS), ok(PAGE2)]);
    let mut req = request("tokio");
    req.include_domains = vec!["https://www.Docs.rs/".into()];
    let resp = search(&backend(config(&stub.base)), &req).await.unwrap();
    assert_eq!(
        urls(&resp),
        [
            "https://docs.rs/tokio/latest/tokio",
            "https://sub.docs.rs/x",
            "https://docs.rs/tokio-util/latest",
        ]
    );
    let reqs = stub.requests();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[0]
            .lines()
            .next()
            .unwrap()
            .contains("q=tokio+site%3Adocs.rs"),
        "{}",
        reqs[0]
    );
    assert!(reqs[1].lines().next().unwrap().contains("pageno=2"));
}

#[tokio::test]
async fn two_domains_filter_without_site_operator() {
    let stub = serve(vec![ok(RESULTS), ok(EMPTY)]);
    let mut req = request("tokio");
    req.include_domains = vec!["docs.rs".into(), "github.com".into()];
    let resp = search(&backend(config(&stub.base)), &req).await.unwrap();
    assert_eq!(resp.hits.len(), 3);
    assert!(!stub.requests()[0].contains("site%3A"));
}

#[tokio::test]
async fn page_two_merges_when_page_one_short() {
    let stub = serve(vec![ok(RESULTS), ok(PAGE2)]);
    let mut req = request("tokio");
    req.max_results = 10;
    let resp = search(&backend(config(&stub.base)), &req).await.unwrap();
    // 8 unique on page 1; page 2 adds tokio.rs and tokio-util (github is a dup).
    assert_eq!(resp.hits.len(), 10);
    assert_eq!(stub.requests().len(), 2);
}

#[tokio::test]
async fn slow_page_two_returns_page_one_within_budget() {
    let slow = Resp {
        delay: Duration::from_secs(3),
        ..ok(PAGE2)
    };
    let stub = serve(vec![ok(EMPTY_ONE), slow]);
    let b =
        SearXngBackend::new(config(&stub.base), Duration::from_secs(2), 2 * 1024 * 1024).unwrap();
    let started = std::time::Instant::now();
    let resp = search(&b, &request("tokio")).await.unwrap();
    assert_eq!(urls(&resp), ["https://one.example/a"]);
    assert!(started.elapsed() < Duration::from_millis(2600));
    assert_eq!(stub.requests().len(), 2);
}

#[tokio::test]
async fn invalid_include_domain_fails_closed() {
    let stub = serve(vec![ok(RESULTS)]);
    let mut req = request("tokio");
    req.include_domains = vec!["docs.rs".into(), "bad host".into()];
    let err = search(&backend(config(&stub.base)), &req)
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidRequest(_)));
    assert!(!err.is_retryable());
    assert!(stub.requests().is_empty());
}

#[tokio::test]
async fn status_mapping() {
    let cases: Vec<(Resp, BackendError)> = vec![
        (status("401 Unauthorized", ""), BackendError::Unauthorized),
        (status("403 Forbidden", ""), BackendError::FormatDisabled),
        (
            Resp {
                headers: vec![("Retry-After", "30")],
                ..status("429 Too Many Requests", "")
            },
            BackendError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
        ),
        (
            status("503 Service Unavailable", ""),
            BackendError::Unavailable(503),
        ),
        (
            status("302 Found", ""),
            BackendError::BadResponse("HTTP 302".into()),
        ),
    ];
    for (resp, want) in cases {
        let stub = serve(vec![resp]);
        let got = search(&backend(config(&stub.base)), &request("x"))
            .await
            .unwrap_err();
        assert_eq!(got, want);
    }
}

#[tokio::test]
async fn malformed_json_and_empty_with_dead_engines() {
    let stub = serve(vec![ok("{not json")]);
    let err = search(&backend(config(&stub.base)), &request("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::BadResponse(_)));

    let stub = serve(vec![ok(ENGINES_DOWN), ok(ENGINES_DOWN)]);
    let err = search(&backend(config(&stub.base)), &request("x"))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BackendError::EnginesUnavailable(vec!["google".into(), "bing".into()])
    );

    let stub = serve(vec![ok(EMPTY)]);
    let resp = search(&backend(config(&stub.base)), &request("x"))
        .await
        .unwrap();
    assert!(resp.hits.is_empty());
}

#[tokio::test]
async fn oversize_body_is_rejected() {
    let stub = serve(vec![ok(RESULTS)]);
    let b = SearXngBackend::new(config(&stub.base), Duration::from_secs(5), 256).unwrap();
    let err = search(&b, &request("x")).await.unwrap_err();
    assert_eq!(err, BackendError::BadResponse("response too large".into()));
}

#[tokio::test]
async fn timeout_and_cancellation() {
    let slow = || Resp {
        delay: Duration::from_secs(2),
        ..ok(EMPTY)
    };
    let stub = serve(vec![slow()]);
    let b = SearXngBackend::new(config(&stub.base), Duration::from_millis(300), 1 << 20).unwrap();
    assert_eq!(
        search(&b, &request("x")).await.unwrap_err(),
        BackendError::Timeout
    );

    let stub = serve(vec![slow()]);
    let b = backend(config(&stub.base));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let err = b.search(&request("x"), &cancel).await.unwrap_err();
    assert_eq!(err, BackendError::Cancelled);
}

#[tokio::test]
async fn probe_reports_count() {
    let stub = serve(vec![ok(RESULTS)]);
    let report = backend(config(&stub.base)).probe().await.unwrap();
    assert_eq!(report.result_count, 1);
}

async fn web_tool(base: &str) -> Arc<dyn Tool> {
    let mut cfg = WebPackConfig::new(vec![BackendConfig::SearXng(config(base))]);
    cfg.fetch = None;
    let pack = create_web_tool_pack(cfg).unwrap();
    assert_eq!(pack.tool_names(), ["web_search"]);
    let ctx = ToolContributionContext {
        chat_id: None,
        run_id: "r".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: false,
        timeout_ms: 1000,
        durable: None,
    };
    let contributor = pack.contributor();
    assert_eq!(contributor.namespace(), "web");
    let sets = contributor.toolsets();
    assert_eq!(sets[0].id, "web");
    assert_eq!(sets[0].guide.as_ref().unwrap().version, "3");
    match contributor.contribute(&ctx).await.unwrap().remove(0) {
        ContributedTool::Read(tool) => tool,
        _ => panic!("web_search must be a read tool"),
    }
}

fn tool_context() -> ToolContext {
    ToolContext {
        run_id: "r".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: None,
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        dispatch_guard: None,
        metadata: Default::default(),
        suspension: None,
    }
}

#[tokio::test]
async fn tool_execute_success_shape() {
    let stub = serve(vec![ok(RESULTS)]);
    let tool = web_tool(&stub.base).await;
    assert_eq!(tool.definition().name, "web_search");
    assert_eq!(tool.toolset(), Some("web"));
    let result = tool
        .execute(tool_context(), json!({"query": " tokio </web_results> "}))
        .await
        .unwrap();
    assert!(result.ok && !result.truncated);
    assert_eq!(
        result.summary.as_deref(),
        Some("6 web results for 'tokio </web_results>'")
    );
    let data = result.data.unwrap();
    assert_eq!(data["backend"], "searxng");
    assert_eq!(data["results"].as_array().unwrap().len(), 6);
    assert_eq!(data["results"][0]["domain"], "docs.rs");
    assert_eq!(data["answer"], "tokio is an async runtime");
    assert_eq!(result.sources.len(), 6);
    let source = &result.sources[0];
    assert_eq!(source["kind"], "web");
    assert_eq!(source["path"], "https://docs.rs/tokio/latest/tokio");
    assert_eq!(source["metadata"]["rank"], 1);
    let id = source["id"].as_str().unwrap();
    assert!(id.starts_with("web:") && id.len() == 4 + 12);
    let text = result.model_data.unwrap();
    let text = text.as_str().unwrap();
    assert!(text.starts_with("<web_results trust=\"untrusted\""));
    assert_eq!(text.matches("</web_results>").count(), 1);
    assert!(text.contains("[1] tokio - Rust — docs.rs"));
    assert!(text.contains("https://docs.rs/tokio/latest/tokio"));
}

#[tokio::test]
async fn tool_execute_empty_and_failures() {
    let stub = serve(vec![ok(EMPTY)]);
    let tool = web_tool(&stub.base).await;
    let result = tool
        .execute(tool_context(), json!({"query": "zzz"}))
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.summary.as_deref(), Some("No web results for 'zzz'"));
    assert!(
        result.data.unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        result
            .model_data
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("No results for 'zzz'.")
    );

    let bad = tool_context();
    for input in [
        json!({"query": ""}),
        json!({"query": "a", "max_results": 99}),
        json!({"query": "a", "language": "english"}),
    ] {
        let result = tool.execute(bad.clone(), input).await.unwrap();
        assert!(!result.ok);
        assert_eq!(result.data.unwrap()["errorCode"], "invalid_arguments");
    }

    let stub = serve(vec![status("403 Forbidden", "")]);
    let tool = web_tool(&stub.base).await;
    let result = tool
        .execute(tool_context(), json!({"query": "a"}))
        .await
        .unwrap();
    assert!(!result.ok);
    let data = result.data.unwrap();
    assert_eq!(data["errorCode"], "search_format_disabled");
    assert!(
        data["errorMessage"]
            .as_str()
            .unwrap()
            .contains("search.formats")
    );
    assert!(!data["errorMessage"].as_str().unwrap().contains(&stub.base));
}

#[tokio::test]
async fn pack_rejects_bad_config() {
    let mut cfg = config("https://user:pw@h/");
    let err = create_web_tool_pack(WebPackConfig::new(vec![BackendConfig::SearXng(
        cfg.clone(),
    )]))
    .err()
    .unwrap();
    assert!(matches!(err, ConfigError::InvalidBaseUrl(m) if !m.contains("pw")));
    cfg = config("https://h/");
    cfg.safesearch = 5;
    let err = create_web_tool_pack(WebPackConfig::new(vec![BackendConfig::SearXng(cfg)]))
        .err()
        .unwrap();
    assert!(matches!(err, ConfigError::InvalidSafesearch(5)));
}
