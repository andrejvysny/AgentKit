//! `web_search` through a two-backend pack: fallback messages and cache warning.

mod common;

use std::time::Duration;

use agentkit_core::{Tool, ToolContext, ToolLimits};
use agentkit_governance::{ContributedTool, ToolContributionContext};
use agentkit_toolpacks_web::*;
use common::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

const BRAVE: &str = include_str!("fixtures/brave/results.json");

fn pack(searx: &str, brave: &str) -> WebToolPack {
    let mut b = BraveConfig::new("brave-key-123");
    b.endpoint = Url::parse(&format!("{brave}/s")).unwrap();
    let mut cfg = WebPackConfig::new(vec![
        BackendConfig::SearXng(SearXngConfig::new(Url::parse(searx).unwrap())),
        BackendConfig::Brave(b),
    ]);
    cfg.request_timeout = Duration::from_secs(5);
    create_web_tool_pack(cfg).unwrap()
}

async fn tool_of(pack: &WebToolPack) -> std::sync::Arc<dyn Tool> {
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
    match pack.contributor().contribute(&ctx).await.unwrap().remove(0) {
        ContributedTool::Read(tool) => tool,
        _ => panic!("web_search must be a read tool"),
    }
}

fn tool_context(run: &str) -> ToolContext {
    ToolContext {
        run_id: run.into(),
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
async fn fallback_warns_and_cache_hit_warns() {
    let searx = serve(vec![status("503 Service Unavailable", "")]);
    let brave = serve(vec![ok(BRAVE)]);
    let tool = tool_of(&pack(&searx.base, &brave.base)).await;
    let first = tool
        .execute(tool_context("r1"), json!({"query": "tokio"}))
        .await
        .unwrap();
    assert!(first.ok);
    assert_eq!(
        first.warnings,
        ["searxng failed: backend unavailable (HTTP 503); answered by brave"]
    );
    let data = first.data.unwrap();
    assert_eq!(data["backend"], "brave");
    assert_eq!(data["cached"], false);
    let text = first.model_data.unwrap();
    assert!(text.as_str().unwrap().contains("backend=\"brave\""));

    let second = tool
        .execute(tool_context("r1"), json!({"query": "Tokio "}))
        .await
        .unwrap();
    assert_eq!(second.warnings, ["Served from this run's cache."]);
    assert_eq!(second.data.unwrap()["cached"], true);
    assert_eq!(brave.requests().len(), 1);
}

#[tokio::test]
async fn all_failed_message_lists_both_backends() {
    let searx = serve(vec![status("429 Too Many Requests", "")]);
    let brave = serve(vec![status("504 Gateway Timeout", "")]);
    let tool = tool_of(&pack(&searx.base, &brave.base)).await;
    let result = tool
        .execute(tool_context("r2"), json!({"query": "x"}))
        .await
        .unwrap();
    assert!(!result.ok);
    let data = result.data.unwrap();
    assert_eq!(data["errorCode"], "search_unavailable");
    let msg = data["errorMessage"].as_str().unwrap();
    assert!(
        msg.contains("(searxng: rate limited; brave: backend unavailable (HTTP 504))"),
        "{msg}"
    );
    assert!(!msg.contains("brave-key-123") && !msg.contains("127.0.0.1"));
}

#[tokio::test]
async fn unauthorized_names_the_backend_and_budget_message() {
    let searx = serve(vec![status("503 Service Unavailable", "")]);
    let brave = serve(vec![status("401 Unauthorized", "")]);
    let tool = tool_of(&pack(&searx.base, &brave.base)).await;
    let result = tool
        .execute(tool_context("r3"), json!({"query": "x"}))
        .await
        .unwrap();
    let data = result.data.unwrap();
    assert_eq!(data["errorCode"], "search_unauthorized");
    assert!(
        data["errorMessage"]
            .as_str()
            .unwrap()
            .starts_with("The brave web search backend rejected the credentials")
    );

    let searx = serve((0..9).map(|_| ok(r#"{"results":[]}"#)).collect());
    let brave = serve(vec![]);
    let tool = tool_of(&pack(&searx.base, &brave.base)).await;
    for i in 0..8 {
        let r = tool
            .execute(tool_context("r4"), json!({"query": format!("q{i}")}))
            .await
            .unwrap();
        assert!(r.ok);
    }
    let result = tool
        .execute(tool_context("r4"), json!({"query": "q8"}))
        .await
        .unwrap();
    assert!(!result.ok);
    let data = result.data.unwrap();
    assert_eq!(data["errorCode"], "search_budget_exhausted");
}

#[tokio::test]
async fn pack_probe_and_config_validation() {
    let brave = serve(vec![ok(BRAVE)]);
    let p = pack("http://127.0.0.1:9", &brave.base);
    assert_eq!(p.probe(BackendId::Brave).await.unwrap().result_count, 1);
    assert_eq!(
        p.probe(BackendId::Exa).await.unwrap_err(),
        BackendError::InvalidRequest("backend not configured".into())
    );
    assert!(p.router().backend(BackendId::SearXng).is_some());

    assert!(matches!(
        create_web_tool_pack(WebPackConfig::new(vec![])).err(),
        Some(ConfigError::NoBackends)
    ));
    let dup = WebPackConfig::new(vec![
        BackendConfig::Exa(ExaConfig::new("a")),
        BackendConfig::Exa(ExaConfig::new("b")),
    ]);
    assert!(matches!(
        create_web_tool_pack(dup).err(),
        Some(ConfigError::DuplicateBackend("exa"))
    ));
}

#[tokio::test]
async fn default_pack_offers_search_and_fetch_sharing_one_budget() {
    let p = pack("http://127.0.0.1:9", "http://127.0.0.1:9");
    assert_eq!(p.tool_names(), ["web_search", "web_fetch"]);
    let search = tool_of(&p).await;
    assert!(search.definition().description.contains("web_fetch"));
    let sets = p.contributor().toolsets();
    assert_eq!(sets[0].tools, ["web_search", "web_fetch"]);
    let router = p.router();
    for _ in 0..12 {
        assert!(router.try_consume_fetch("run-budget"));
    }
    assert!(!router.try_consume_fetch("run-budget"));
}
