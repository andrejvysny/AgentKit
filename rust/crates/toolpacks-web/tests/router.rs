//! BackendRouter against scripted in-test backends.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_toolpacks_web::*;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

struct Fake {
    id: BackendId,
    script: Mutex<VecDeque<Result<(), BackendError>>>,
    delay: Duration,
    calls: Mutex<u32>,
}

impl Fake {
    fn new(id: BackendId, script: Vec<Result<(), BackendError>>) -> Arc<Self> {
        Arc::new(Self {
            id,
            script: Mutex::new(script.into()),
            delay: Duration::ZERO,
            calls: Mutex::new(0),
        })
    }

    fn slow(id: BackendId, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            id,
            script: Mutex::new(VecDeque::new()),
            delay,
            calls: Mutex::new(0),
        })
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

fn response(id: BackendId) -> SearchResponse {
    SearchResponse {
        hits: vec![SearchHit {
            url: Url::parse("https://example.com/a").unwrap(),
            title: "t".into(),
            snippet: "s".into(),
            published: None,
            extra: vec![],
            engine: id.as_str().into(),
        }],
        backend: id,
        suggestions: vec![],
        answer: None,
    }
}

#[async_trait]
impl SearchBackend for Fake {
    fn id(&self) -> BackendId {
        self.id
    }

    async fn search(
        &self,
        _req: &SearchRequest,
        _cancel: &CancellationToken,
    ) -> Result<SearchResponse, BackendError> {
        *self.calls.lock().unwrap() += 1;
        tokio::time::sleep(self.delay).await;
        let next = self.script.lock().unwrap().pop_front();
        next.unwrap_or(Ok(())).map(|()| response(self.id))
    }

    async fn probe(&self) -> Result<ProbeReport, BackendError> {
        Ok(ProbeReport {
            latency_ms: 0,
            result_count: 0,
        })
    }
}

fn req(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        max_results: 6,
        freshness: None,
        include_domains: vec![],
        language: None,
    }
}

fn router(backends: Vec<Arc<Fake>>) -> BackendRouter {
    let list: Vec<Arc<dyn SearchBackend>> = backends
        .into_iter()
        .map(|b| b as Arc<dyn SearchBackend>)
        .collect();
    BackendRouter::new(
        list,
        RunLimits::default(),
        CacheConfig::default(),
        Duration::from_secs(5),
    )
}

async fn go(r: &BackendRouter, run: &str, query: &str) -> Result<Routed, RouterError> {
    r.search(run, &req(query), &CancellationToken::new()).await
}

#[tokio::test]
async fn falls_back_in_order_and_records_failures() {
    let a = Fake::new(BackendId::SearXng, vec![Err(BackendError::Timeout)]);
    let b = Fake::new(BackendId::Brave, vec![Err(BackendError::QuotaExhausted)]);
    let c = Fake::new(BackendId::Exa, vec![Ok(())]);
    let r = router(vec![a.clone(), b.clone(), c.clone()]);
    let routed = go(&r, "run", "q").await.unwrap();
    assert!(!routed.cached);
    assert_eq!(routed.response.backend, BackendId::Exa);
    assert_eq!(
        routed.failed,
        vec![
            (BackendId::SearXng, BackendError::Timeout),
            (BackendId::Brave, BackendError::QuotaExhausted)
        ]
    );
    assert_eq!((a.calls(), b.calls(), c.calls()), (1, 1, 1));
}

#[tokio::test]
async fn continues_on_unauthorized_and_rejected() {
    for error in [
        BackendError::Unauthorized,
        BackendError::Rejected("no".into()),
        BackendError::FormatDisabled,
    ] {
        let a = Fake::new(BackendId::SearXng, vec![Err(error.clone())]);
        let b = Fake::new(BackendId::Brave, vec![Ok(())]);
        let r = router(vec![a, b.clone()]);
        let routed = go(&r, "run", "q").await.unwrap();
        assert_eq!(routed.failed, vec![(BackendId::SearXng, error)]);
        assert_eq!(b.calls(), 1);
    }
}

#[tokio::test]
async fn stops_on_invalid_request() {
    let a = Fake::new(
        BackendId::SearXng,
        vec![Err(BackendError::InvalidRequest("bad".into()))],
    );
    let b = Fake::new(BackendId::Brave, vec![Ok(())]);
    let r = router(vec![a, b.clone()]);
    match go(&r, "run", "q").await {
        Err(RouterError::AllFailed(failed)) => {
            assert_eq!(
                failed,
                vec![(
                    BackendId::SearXng,
                    BackendError::InvalidRequest("bad".into())
                )]
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(b.calls(), 0);
}

#[tokio::test]
async fn all_failed_lists_every_backend() {
    let a = Fake::new(
        BackendId::SearXng,
        vec![Err(BackendError::Unavailable(503))],
    );
    let b = Fake::new(BackendId::Brave, vec![Err(BackendError::Timeout)]);
    let r = router(vec![a, b]);
    match go(&r, "run", "q").await {
        Err(RouterError::AllFailed(failed)) => assert_eq!(failed.len(), 2),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn budget_exhausts_after_eight_searches() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let r = router(vec![a.clone()]);
    for i in 0..8 {
        go(&r, "run", &format!("q{i}")).await.unwrap();
    }
    assert!(matches!(
        go(&r, "run", "q8").await,
        Err(RouterError::BudgetExhausted)
    ));
    assert_eq!(a.calls(), 8);
    go(&r, "other", "q8").await.unwrap();
}

#[tokio::test]
async fn cache_hit_does_not_consume_budget() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let r = router(vec![a.clone()]);
    go(&r, "run", "Tokio Runtime").await.unwrap();
    for _ in 0..20 {
        let hit = go(&r, "run", "  tokio runtime ").await.unwrap();
        assert!(hit.cached && hit.failed.is_empty());
    }
    assert_eq!(a.calls(), 1);
    for i in 0..7 {
        go(&r, "run", &format!("other{i}")).await.unwrap();
    }
    assert!(matches!(
        go(&r, "run", "one more").await,
        Err(RouterError::BudgetExhausted)
    ));
    assert!(go(&r, "run", "tokio runtime").await.unwrap().cached);
}

#[tokio::test]
async fn cache_key_separates_parameters() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let r = router(vec![a.clone()]);
    go(&r, "run", "q").await.unwrap();
    let mut other = req("q");
    other.freshness = Some(Freshness::Day);
    let routed = r
        .search("run", &other, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!routed.cached);
    let mut domains = req("q");
    domains.include_domains = vec!["b.com".into(), "a.com".into()];
    r.search("run", &domains, &CancellationToken::new())
        .await
        .unwrap();
    domains.include_domains = vec!["a.com".into(), "b.com".into()];
    let routed = r
        .search("run", &domains, &CancellationToken::new())
        .await
        .unwrap();
    assert!(routed.cached);
}

#[tokio::test]
async fn cache_expires_after_ttl() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let cache = CacheConfig {
        ttl: Duration::from_millis(50),
        ..CacheConfig::default()
    };
    let r = BackendRouter::new(
        vec![a.clone() as Arc<dyn SearchBackend>],
        RunLimits::default(),
        cache,
        Duration::from_secs(5),
    );
    go(&r, "run", "q").await.unwrap();
    assert!(go(&r, "run", "q").await.unwrap().cached);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!go(&r, "run", "q").await.unwrap().cached);
    assert_eq!(a.calls(), 2);
}

#[tokio::test]
async fn lru_evicts_least_recently_used_run() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let cache = CacheConfig {
        max_runs: 2,
        ..CacheConfig::default()
    };
    let r = BackendRouter::new(
        vec![a.clone() as Arc<dyn SearchBackend>],
        RunLimits::default(),
        cache,
        Duration::from_secs(5),
    );
    go(&r, "a", "q").await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    go(&r, "b", "q").await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(go(&r, "a", "q").await.unwrap().cached);
    tokio::time::sleep(Duration::from_millis(5)).await;
    go(&r, "c", "q").await.unwrap();
    assert!(go(&r, "a", "q").await.unwrap().cached);
    assert!(!go(&r, "b", "q").await.unwrap().cached, "b was evicted");
}

#[tokio::test]
async fn total_deadline_spans_slow_backends() {
    let a = Fake::slow(BackendId::SearXng, Duration::from_secs(10));
    let b = Fake::slow(BackendId::Brave, Duration::from_secs(10));
    let c = Fake::new(BackendId::Exa, vec![Ok(())]);
    let r = BackendRouter::new(
        vec![
            a.clone() as Arc<dyn SearchBackend>,
            b.clone() as Arc<dyn SearchBackend>,
            c.clone() as Arc<dyn SearchBackend>,
        ],
        RunLimits::default(),
        CacheConfig::default(),
        Duration::from_millis(150),
    );
    let started = std::time::Instant::now();
    match go(&r, "run", "q").await {
        Err(RouterError::AllFailed(failed)) => {
            assert_eq!(failed, vec![(BackendId::SearXng, BackendError::Timeout)]);
        }
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!((b.calls(), c.calls()), (0, 0));
}

#[tokio::test]
async fn fetch_budget_and_backend_lookup() {
    let a = Fake::new(BackendId::SearXng, vec![]);
    let r = router(vec![a]);
    for _ in 0..12 {
        assert!(r.try_consume_fetch("run"));
    }
    assert!(!r.try_consume_fetch("run"));
    assert!(r.try_consume_fetch("other"));
    assert!(r.backend(BackendId::SearXng).is_some());
    assert!(r.backend(BackendId::Exa).is_none());
}
