//! Prompt caching in a plain chat (hardening H1.3, B3). The executor plans
//! cache directives from the provider's declared capability for every turn,
//! not only session turns: an implicit-prefix provider (OpenAI) gets a stable
//! `prompt_cache_key`, and a breakpoint provider (Anthropic) gets its four
//! breakpoints once the chat has a previous request to cache.

use agentkit_chat::*;
use agentkit_core::{
    BreakpointAt, CacheCapability, CacheDirectives, CacheHint, CacheTtl, ChatProvider, ChatRequest,
    CoreError, ProviderEvent, ProviderStream, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// Answers with text and records each request's cache directives.
struct Recording {
    cache: CacheCapability,
    seen: Arc<Mutex<Vec<Option<CacheDirectives>>>>,
}

#[async_trait]
impl ChatProvider for Recording {
    fn id(&self) -> &str {
        "test"
    }
    fn cache_capability(&self) -> CacheCapability {
        self.cache.clone()
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.seen.lock().unwrap().push(input.cache.clone());
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

struct Lookup(ToolDefinition);

#[async_trait]
impl Tool for Lookup {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: None,
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

struct SystemPrompt;

#[async_trait]
impl ContextProvider for SystemPrompt {
    async fn resolve(&self, _: Value, _: CancellationToken) -> ChatResult<Value> {
        Ok(json!({"systemPrompt": "You are a careful assistant.", "bindings": []}))
    }
}

/// Two plain-chat turns on a provider with `cache`; returns the directives
/// each request carried.
async fn two_turns(cache: CacheCapability) -> Vec<Option<CacheDirectives>> {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Lookup(
            serde_json::from_value(json!({
                "name": "lookup", "version": "1", "effect": "read",
                "capability": "test.read", "description": "look something up",
                "inputSchema": {"type": "object"},
            }))
            .unwrap(),
        )))
        .unwrap();
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::new(Recording {
                cache,
                seen: seen.clone(),
            }),
            Arc::new(tools),
        )
        .with_options(ChatExecutorOptions {
            context: Some(Arc::new(SystemPrompt)),
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
    let mut parent = None;
    for turn in ["t1", "t2"] {
        host.submit_chat(SubmitChat {
            task_id: turn.into(),
            chat_id: "chat".into(),
            model: "model".into(),
            content: json!(format!("question {turn}")),
            parent_message_id: parent.take(),
        })
        .await
        .unwrap();
        host.run_once().await.unwrap();
        parent = Some(format!("{turn}_assistant"));
    }
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one request per turn");
    requests
}

#[tokio::test]
async fn an_implicit_prefix_provider_gets_a_stable_cache_key_in_a_plain_chat() {
    let seen = two_turns(CacheCapability::ImplicitPrefix {
        hint: Some(CacheHint::PromptCacheKey),
    })
    .await;
    let keys: Vec<_> = seen
        .iter()
        .map(|cache| {
            let cache = cache.as_ref().expect("cache directives on every request");
            assert_eq!(cache.key_field, Some(CacheHint::PromptCacheKey));
            cache.cache_key.clone().expect("a cache key")
        })
        .collect();
    assert_eq!(
        keys[0], keys[1],
        "the key is stable across the chat's turns"
    );
}

#[tokio::test]
async fn a_breakpoint_provider_gets_four_breakpoints_in_a_plain_chat() {
    let seen = two_turns(CacheCapability::ExplicitBreakpoints {
        max_breakpoints: 4,
        ttls: vec![CacheTtl::FiveMinutes, CacheTtl::OneHour],
        min_tokens: 0,
    })
    .await;
    let second = seen[1].as_ref().expect("cache directives");
    let sites: Vec<_> = second.breakpoints.iter().map(|point| point.at).collect();
    assert_eq!(sites.len(), 4, "{sites:?}");
    assert_eq!(sites[0], BreakpointAt::LastTool);
    assert!(
        matches!(sites[1], BreakpointAt::EndOfSegment(_)),
        "{sites:?}"
    );
    assert!(
        matches!(sites[2], BreakpointAt::AfterMessage(_)),
        "{sites:?}"
    );
    assert!(
        matches!(sites[3], BreakpointAt::AfterMessage(_)),
        "{sites:?}"
    );
}

#[tokio::test]
async fn a_provider_without_caching_gets_no_directives() {
    let seen = two_turns(CacheCapability::None).await;
    assert!(seen.iter().all(Option::is_none), "{seen:?}");
}
