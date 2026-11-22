//! The web pack inside AgentKit's real chat loop: scripted provider, real
//! executor and store, 127.0.0.1 SearXNG stub. No network, no model.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_chat::*;
use agentkit_core::{
    ChatMessage, ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolCall, ToolRegistry,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_governance::ToolGuard;
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use agentkit_toolpacks_web::*;
use async_trait::async_trait;
use common::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

const CHAT: &str = "chat";
const TURN: &str = "t1";
const RESULTS: &str = include_str!("fixtures/searxng/results.json");
const STUB_URL: &str = "https://docs.rs/tokio/latest/tokio";

/// Turn 1 calls `web_search` once per query; every later turn answers.
struct Scripted {
    queries: Vec<&'static str>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(input);
            requests.len() == 1
        };
        let calls = if first {
            self.queries
                .iter()
                .enumerate()
                .map(|(i, query)| ToolCall {
                    id: format!("call-{}", i + 1),
                    name: "web_search".into(),
                    arguments_json: json!({ "query": query }).to_string(),
                })
                .collect()
        } else {
            vec![]
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if first { "" } else { "done" }.into(),
            finish_reason: if first { "tool_calls" } else { "stop" }.into(),
            tool_calls: calls,
        })]))
    }
}

struct Fixture {
    store: SqliteStore,
    _host: AgentHost,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

async fn fixture(
    stub: &Stub,
    queries: Vec<&'static str>,
    limits: RunLimits,
    guards: Vec<Arc<dyn ToolGuard>>,
) -> Fixture {
    let mut cfg = WebPackConfig::new(vec![BackendConfig::SearXng(SearXngConfig::new(
        Url::parse(&stub.base).unwrap(),
    ))]);
    cfg.request_timeout = Duration::from_secs(5);
    cfg.fetch = None;
    cfg.limits = limits;
    let pack = create_web_tool_pack(cfg).unwrap();
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat(CHAT, None, json!({}), 0).await.unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        AgentExecutor::new(
            Arc::new(Scripted {
                queries,
                requests: requests.clone(),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(AgentExecutorOptions {
            tool_contributors: vec![pack.contributor()],
            tool_guards: guards,
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: TURN.into(),
        chat_id: CHAT.into(),
        model: "model".into(),
        content: json!("search please"),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    Fixture {
        store,
        _host: host,
        requests,
    }
}

impl Fixture {
    async fn status(&self) -> TaskStatus {
        self.store.get_task(TURN).await.unwrap().unwrap().status
    }

    /// Tool messages of the second provider request, in order.
    fn tool_messages(&self) -> Vec<ChatMessage> {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one request before the tools, one after");
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .cloned()
            .collect()
    }

    async fn events(&self, kind: &str) -> Vec<Value> {
        self.store
            .events(TURN, None, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .filter(|e| e["type"] == kind)
            .collect()
    }
}

/// The tool message as the envelope JSON the model reads.
fn envelope(message: &ChatMessage) -> Value {
    match &message.content {
        Value::String(s) => serde_json::from_str(s).expect("envelope json"),
        other => other.clone(),
    }
}

/// The text the model reads: the rendered block or the failure message/code.
fn text(message: &ChatMessage) -> String {
    envelope(message).to_string()
}

#[tokio::test]
async fn search_result_reaches_model_as_untrusted_block() {
    let stub = serve(vec![ok(RESULTS)]);
    let f = fixture(&stub, vec!["tauri updater"], RunLimits::default(), vec![]).await;
    assert_eq!(f.status().await, TaskStatus::Completed);
    let tools = f.tool_messages();
    assert_eq!(tools.len(), 1);
    let seen = text(&tools[0]);
    let block = envelope(&tools[0])["data"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(block.contains("<web_results trust=\"untrusted\""), "{seen}");
    assert!(block.contains(STUB_URL), "{seen}");
    assert_eq!(stub.requests().len(), 1);
    let done = f.events("run.tool.succeeded").await;
    assert_eq!(done.len(), 1, "{done:?}");
    let sources = done[0]["data"]["sources"].as_array().expect("sources");
    assert!(!sources.is_empty(), "{done:?}");
    assert!(sources.iter().all(|s| s["kind"] == "web"), "{sources:?}");
    assert!(sources.iter().any(|s| s["path"] == STUB_URL), "{sources:?}");
}

#[tokio::test]
async fn web_guard_ask_parks_the_run() {
    let stub = serve(vec![ok(RESULTS)]);
    let policy = NetPolicy {
        search: NetDecision::Ask,
        fetch: NetDecision::Allow,
    };
    let f = fixture(
        &stub,
        vec!["tauri updater"],
        RunLimits::default(),
        vec![Arc::new(WebGuard::new(policy))],
    )
    .await;
    assert_eq!(f.status().await, TaskStatus::WaitingApproval);
    assert!(stub.requests().is_empty(), "parked before any egress");
}

/// The chat loop keeps a guard-hidden tool in the offered definitions and
/// refuses the call as "not available this turn"; no request reaches the stub.
#[tokio::test]
async fn web_guard_deny_withholds_tool() {
    let stub = serve(vec![]);
    let f = fixture(
        &stub,
        vec!["tauri updater"],
        RunLimits::default(),
        vec![Arc::new(WebGuard::new(NetPolicy::deny_all()))],
    )
    .await;
    assert_eq!(f.status().await, TaskStatus::Completed);
    let tools = f.tool_messages();
    assert_eq!(tools.len(), 1);
    let refused = envelope(&tools[0]);
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(
        refused["data"]["errorCode"], "tool_not_allowed",
        "{refused}"
    );
    assert!(stub.requests().is_empty(), "denied before any egress");
}

fn tool_for<'a>(tools: &'a [ChatMessage], id: &str) -> &'a ChatMessage {
    tools
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no tool message for {id}"))
}

#[tokio::test]
async fn parallel_searches_share_budget() {
    let queries = vec!["alpha", "beta", "gamma"];

    let stub = serve(vec![ok(RESULTS), ok(RESULTS), ok(RESULTS)]);
    let f = fixture(&stub, queries.clone(), RunLimits::default(), vec![]).await;
    assert_eq!(f.status().await, TaskStatus::Completed);
    let tools = f.tool_messages();
    let ids: Vec<_> = tools
        .iter()
        .map(|m| m.tool_call_id.clone().unwrap())
        .collect();
    assert_eq!(ids, ["call-1", "call-2", "call-3"], "call order");
    for (i, query) in queries.iter().enumerate() {
        let block = envelope(&tools[i])["data"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(block.contains(&format!("query=\"{query}\"")), "{block}");
    }
    assert_eq!(stub.requests().len(), 3);

    let stub = serve(vec![ok(RESULTS), ok(RESULTS)]);
    let limits = RunLimits {
        searches_per_run: 2,
        ..RunLimits::default()
    };
    let f = fixture(&stub, queries, limits, vec![]).await;
    assert_eq!(f.status().await, TaskStatus::Completed);
    let tools = f.tool_messages();
    assert_eq!(tools.len(), 3);
    for id in ["call-1", "call-2"] {
        assert_eq!(envelope(tool_for(&tools, id))["ok"], true);
    }
    let third = envelope(tool_for(&tools, "call-3"));
    assert_eq!(third["ok"], false, "{third}");
    assert_eq!(
        third["data"]["errorCode"], "search_budget_exhausted",
        "{third}"
    );
    assert_eq!(stub.requests().len(), 2);
}
