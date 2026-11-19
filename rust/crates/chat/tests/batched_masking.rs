//! CE-13 (roadmap 2.4, ADR 0034): in a 60-turn tool-heavy session the cached
//! prefix breaks only at the few points where batched masking (or compaction)
//! rewrote it, and every masked result stays readable through its handle.
#![cfg(unix)]

use agentkit_chat::*;
use agentkit_core::{
    ByteOrLineRange, ChatMessage, ChatProvider, ChatRequest, CoreError, Match, OutputError,
    OutputGate, OutputHandle, OutputInfo, OutputMeta, OutputSlice, OutputStore, ProviderEvent,
    ProviderStream, PurgeReport, Runtime, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use agentkit_testkit::{InMemoryOutputStore, ManualClock, cache_provider::CacheSimulatingProvider};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

const TURNS: usize = 60;

struct Counter(AtomicU64);
impl Runtime for Counter {
    fn now_ms(&self) -> i64 {
        1_000
    }
    fn id(&self) -> String {
        format!("call-{}", self.0.fetch_add(1, Ordering::SeqCst))
    }
}

/// What the provider billed for one request.
#[derive(Clone, Copy)]
struct Billed {
    prompt: u64,
    cached: u64,
}

/// Cache simulation underneath; the first request of a turn (last message is
/// the user's) asks for `read_file`, the next one answers.
struct ToolHeavy {
    cache: CacheSimulatingProvider,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    billed: Arc<Mutex<Vec<Billed>>>,
}

#[async_trait]
impl ChatProvider for ToolHeavy {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let ask = !input.tools.is_empty()
            && input
                .messages
                .last()
                .is_some_and(|message| message.role == "user");
        self.requests.lock().unwrap().push(input.clone());
        let billed = self.billed.clone();
        Box::pin(
            self.cache
                .stream_chat(input, cancel)
                .map(move |event| match event? {
                    ProviderEvent::Usage(usage) => {
                        billed.lock().unwrap().push(Billed {
                            prompt: usage["promptTokens"].as_u64().unwrap_or(0),
                            cached: usage["cachedPromptTokens"].as_u64().unwrap_or(0),
                        });
                        Ok(ProviderEvent::Usage(usage))
                    }
                    ProviderEvent::Completed { .. } if ask => Ok(ProviderEvent::Completed {
                        content: String::new(),
                        finish_reason: "tool_calls".into(),
                        tool_calls: vec![agentkit_core::ToolCall {
                            id: "call".into(),
                            name: "read_file".into(),
                            arguments_json: "{\"path\":\"a.txt\"}".into(),
                        }],
                    }),
                    ProviderEvent::Completed { .. } => Ok(ProviderEvent::Completed {
                        content: "done".into(),
                        finish_reason: "stop".into(),
                        tool_calls: vec![],
                    }),
                    other => Ok(other),
                }),
        )
    }
}

/// Each call returns about 5 KiB, numbered so a read-back can be recognised.
struct ReadFile {
    definition: ToolDefinition,
    calls: AtomicUsize,
    /// A handle of another output the result text quotes at its end.
    quote: Option<String>,
}

/// An output store that holds nothing: every write is refused.
struct Refusing(Arc<InMemoryOutputStore>);

#[async_trait]
impl OutputStore for Refusing {
    async fn put(&self, _: OutputMeta, _: Vec<u8>) -> Result<OutputHandle, OutputError> {
        Err(OutputError::Storage("refused".into()))
    }
    async fn info(&self, handle: &OutputHandle) -> Result<OutputInfo, OutputError> {
        self.0.info(handle).await
    }
    async fn read(
        &self,
        handle: &OutputHandle,
        range: ByteOrLineRange,
    ) -> Result<OutputSlice, OutputError> {
        self.0.read(handle, range).await
    }
    async fn search(
        &self,
        handle: &OutputHandle,
        pattern: &str,
        max_matches: u32,
    ) -> Result<Vec<Match>, OutputError> {
        self.0.search(handle, pattern, max_matches).await
    }
    async fn purge_session(&self, session: &str) -> Result<PurgeReport, OutputError> {
        self.0.purge_session(session).await
    }
}

#[derive(Default)]
struct Setup {
    /// The tool's results quote a handle that belongs to another call.
    quote_foreign_handle: bool,
    /// The output store refuses every write.
    refuse_writes: bool,
}

#[async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let quote = self
            .quote
            .as_ref()
            .map_or(String::new(), |handle| format!("see handle {handle}\n"));
        let body = format!(
            "RESULT-{n}\n{quote}{}",
            "the quick brown fox jumps over the lazy dog\n".repeat(110)
        );
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"content": body})),
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

struct ReadContributor(Arc<dyn Tool>);

#[async_trait]
impl ToolContributor for ReadContributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::Read(self.0.clone())])
    }
}

struct Session {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    billed: Arc<Mutex<Vec<Billed>>>,
    outputs: Arc<InMemoryOutputStore>,
}

async fn session(context: Value) -> Session {
    session_with(context, Setup::default()).await
}

async fn session_with(context: Value, setup: Setup) -> Session {
    let requests = Arc::new(Mutex::new(vec![]));
    let billed = Arc::new(Mutex::new(vec![]));
    let outputs = Arc::new(InMemoryOutputStore::new(ManualClock::new(
        1_760_000_000_000,
    )));
    // A real output of this session, made by some other call: the thing a
    // careless result could quote.
    let foreign = if setup.quote_foreign_handle {
        let meta = OutputMeta {
            session: "s1".into(),
            task: "other-task".into(),
            call_id: "other-call".into(),
            tool: "other_tool".into(),
            mime: "text/plain".into(),
            exit_code: None,
        };
        Some(
            outputs
                .put(meta, b"FOREIGN OUTPUT".to_vec())
                .await
                .unwrap()
                .to_string(),
        )
    } else {
        None
    };
    let store_port: Arc<dyn OutputStore> = if setup.refuse_writes {
        Arc::new(Refusing(outputs.clone()))
    } else {
        outputs.clone()
    };
    let tool: Arc<dyn Tool> = Arc::new(ReadFile {
        definition: serde_json::from_value(json!({
            "name": "read_file", "version": "1", "effect": "read",
            "capability": "fs.read", "description": "read",
            "inputSchema": {"type": "object"},
        }))
        .unwrap(),
        calls: AtomicUsize::new(0),
        quote: foreign,
    });
    let provider = ToolHeavy {
        cache: CacheSimulatingProvider::new(
            Arc::new(Counter(AtomicU64::new(0))),
            3_600_000,
            1,
            |_| "session".into(),
        ),
        requests: requests.clone(),
        billed: billed.clone(),
    };
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        AgentExecutor::new(Arc::new(provider), Arc::new(ToolRegistry::new())).with_options(
            AgentExecutorOptions {
                auto_compaction: true,
                tool_contributors: vec![Arc::new(ReadContributor(tool))],
                output: Some(Arc::new(OutputGate::new(store_port))),
                ..Default::default()
            },
        ),
    );
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    })
    .with_profiles(Arc::new(
        ProfileRegistry::new(&[json!({"id":"coding","instructions":"","tools":["read_file"]})])
            .unwrap(),
    ));
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: Some(context),
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    Session {
        store,
        host,
        service,
        requests,
        billed,
        outputs,
    }
}

impl Session {
    async fn turn(&self, text: String) -> String {
        let sent = self
            .service
            .send(
                "s1",
                &SendInput {
                    frozen_payload: None,
                    content: json!(text),
                    input_id: None,
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        let AgentSendResult::Turn { turn_id, .. } = sent else {
            panic!("an idle session starts the turn")
        };
        self.host.run_once().await.unwrap();
        let task = self.store.get_task(&turn_id).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Completed, "turn {turn_id}");
        turn_id
    }

    async fn cache_breaks(&self, turn_id: &str) -> Vec<Value> {
        self.store
            .events_after_number(turn_id, None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.event["type"] == "context.cache_break")
            .map(|event| event.event["data"].clone())
            .collect()
    }
}

fn is_prefix(old: &[ChatMessage], new: &[ChatMessage]) -> bool {
    new.len() >= old.len() && old.iter().zip(new).all(|(a, b)| a == b)
}

#[tokio::test]
async fn sixty_tool_heavy_turns_break_the_cache_only_at_mask_points() {
    let s = session(json!({"windowTokens":40000,"reserveTokens":4000})).await;
    let mut breaks_per_turn = vec![];
    for turn in 0..TURNS {
        let id = s.turn(format!("question {turn}")).await;
        breaks_per_turn.push(s.cache_breaks(&id).await);
    }
    // One tool round and one answer per turn.
    let requests = s.requests.lock().unwrap().clone();
    let billed = s.billed.lock().unwrap().clone();
    assert_eq!(requests.len(), TURNS * 2);
    assert_eq!(billed.len(), TURNS * 2);

    // Only mask or compact breaks, and few of them: the point of batching.
    let broke: Vec<usize> = (0..TURNS)
        .filter(|turn| !breaks_per_turn[*turn].is_empty())
        .collect();
    assert!(
        !broke.is_empty(),
        "60 tool-heavy turns must fill 40k tokens"
    );
    assert!(broke.len() <= 12, "masking is rare, got {broke:?}");
    for data in breaks_per_turn.iter().flatten() {
        let reason = data["reason"].as_str().unwrap();
        assert!(matches!(reason, "mask" | "compact"), "unexpected {data}");
        assert!(data["tokens_after"].as_u64() < data["tokens_before"].as_u64());
    }

    // Every turn that did not break extends the previous turn's request
    // byte for byte, and the simulated provider cache confirms it.
    for turn in 1..TURNS {
        if broke.contains(&turn) {
            continue;
        }
        let (old, new) = (&requests[(turn - 1) * 2], &requests[turn * 2]);
        assert!(
            is_prefix(&old.messages, &new.messages),
            "turn {turn} changed the prefix without a cache_break event"
        );
        let (before, now) = (billed[(turn - 1) * 2], billed[turn * 2]);
        assert!(
            now.cached + 16 >= before.prompt,
            "turn {turn} read {} cached tokens of a {} token prefix",
            now.cached,
            before.prompt
        );
    }

    // Each batch left one durable record, and nothing was compacted.
    let masks = s.store.list_context_masks("s1").await.unwrap();
    assert_eq!(masks.len(), broke.len());
    assert!(
        s.store
            .list_context_summaries("s1")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        breaks_per_turn
            .iter()
            .flatten()
            .all(|d| d["reason"] == "mask")
    );
}

#[tokio::test]
async fn masked_results_stay_readable_through_their_handle() {
    let s = session(json!({"windowTokens":40000,"reserveTokens":4000})).await;
    for turn in 0..TURNS {
        s.turn(format!("question {turn}")).await;
    }
    let requests = s.requests.lock().unwrap().clone();
    let last = requests.last().unwrap();
    let masked: Vec<&ChatMessage> = last
        .messages
        .iter()
        .filter(|message| {
            message.role == "tool"
                && message
                    .content
                    .as_str()
                    .is_some_and(|text| text.starts_with("[result masked to save context · "))
        })
        .collect();
    assert!(masked.len() >= 10, "got {} masked results", masked.len());
    // The newest three turns (one tool result each) are specifically intact.
    let results: Vec<&ChatMessage> = last.messages.iter().filter(|m| m.role == "tool").collect();
    for newest in &results[results.len() - 3..] {
        let text = newest.content.to_string();
        assert!(!text.contains("result masked to save context"), "{text}");
        assert!(text.contains("RESULT-"), "{text}");
    }
    for message in masked {
        let body = message.content.as_str().unwrap();
        let handle = body
            .split("handle ")
            .nth(1)
            .and_then(|rest| rest.strip_suffix(']'))
            .expect("the body names its handle");
        let bytes: u64 = body
            .split(" · ")
            .nth(1)
            .and_then(|part| part.strip_suffix(" bytes"))
            .and_then(|count| count.parse().ok())
            .expect("the body names its size");
        let handle = OutputHandle::parse(handle).unwrap();
        let info = s.outputs.info(&handle).await.unwrap();
        assert_eq!(info.byte_len, bytes);
        assert_eq!(info.meta.session, "s1");
        assert_eq!(info.meta.tool, "read_file");
        let slice = s
            .outputs
            .read(
                &handle,
                ByteOrLineRange::Lines {
                    start_line: 1,
                    max_lines: 5,
                },
            )
            .await
            .unwrap();
        assert!(slice.text.contains("RESULT-"), "{}", slice.text);
        assert_eq!(slice.total_bytes, bytes);
    }
}

#[tokio::test]
async fn a_result_quoting_another_calls_handle_gets_its_own_output() {
    let s = session_with(
        json!({"windowTokens":40000,"reserveTokens":4000}),
        Setup {
            quote_foreign_handle: true,
            ..Setup::default()
        },
    )
    .await;
    for turn in 0..TURNS {
        s.turn(format!("question {turn}")).await;
    }
    let quoted = s
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| {
            m.content
                .as_str()?
                .split("see handle ")
                .nth(1)
                .map(str::to_owned)
        })
        .find_map(|rest| OutputHandle::parse(rest.get(..36)?).ok())
        .expect("the tool result quotes a handle");
    let foreign = s.outputs.info(&quoted).await.unwrap();
    assert_eq!(foreign.meta.call_id, "other-call");
    let masks = s.store.list_context_masks("s1").await.unwrap();
    assert!(!masks.is_empty());
    for mask in masks
        .iter()
        .flat_map(|record| record["masks"].as_array().unwrap())
    {
        let handle = OutputHandle::parse(mask["handle"].as_str().unwrap()).unwrap();
        assert_ne!(handle, foreign.handle, "a quoted handle must not be reused");
        let info = s.outputs.info(&handle).await.unwrap();
        assert_eq!(info.meta.call_id, "call");
        assert_eq!(info.byte_len, mask["bytes"].as_u64().unwrap());
    }
}

#[tokio::test]
async fn when_the_store_refuses_every_write_the_run_compacts_instead() {
    let s = session_with(
        json!({"windowTokens":40000,"reserveTokens":4000}),
        Setup {
            refuse_writes: true,
            ..Setup::default()
        },
    )
    .await;
    let mut reasons = vec![];
    for turn in 0..TURNS {
        let id = s.turn(format!("question {turn}")).await;
        reasons.extend(
            s.cache_breaks(&id)
                .await
                .into_iter()
                .map(|data| data["reason"].as_str().unwrap().to_owned()),
        );
    }
    assert!(s.store.list_context_masks("s1").await.unwrap().is_empty());
    assert!(
        !s.store
            .list_context_summaries("s1")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(reasons.contains(&"compact".to_owned()), "{reasons:?}");
    assert!(!reasons.contains(&"mask".to_owned()), "{reasons:?}");
}

#[tokio::test]
async fn a_record_that_no_longer_matches_is_reported_not_applied() {
    let s = session(json!({"windowTokens":40000,"reserveTokens":4000})).await;
    s.turn("question 0".into()).await;
    s.store
        .save_context_mask(json!({
            "maskId":"stale","sessionId":"s1","coveredCallIds":["call"],
            "masks":[{"messageId":"gone","callId":"call","handle":"out_0123456789abcdef0123456789abcdef","bytes":9}],
            "images":[],"sourceHash":"not-the-hash","tokensBefore":2,"tokensAfter":1,
            "createdAt":"2026-11-15T00:00:00.000Z"
        }))
        .await
        .unwrap();
    let id = s.turn("question 1".into()).await;
    let breaks = s.cache_breaks(&id).await;
    assert_eq!(breaks.len(), 1, "{breaks:?}");
    assert_eq!(breaks[0]["reason"], "mask_invalidated");
    let warned = s
        .store
        .events_after_number(&id, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .any(|event| {
            event.event["type"] == "run.warning"
                && event.event["data"]["code"] == "mask_invalidated"
        });
    assert!(warned);
    // Nothing was half-applied: the request still carries the full results.
    let requests = s.requests.lock().unwrap().clone();
    let last = requests.last().unwrap();
    assert!(last.messages.iter().all(|m| {
        !m.content
            .to_string()
            .contains("result masked to save context")
    }));
}

#[tokio::test]
async fn the_legacy_elision_opt_out_turns_masking_off() {
    let s =
        session(json!({"windowTokens":40000,"reserveTokens":4000,"toolResultElision":false})).await;
    for turn in 0..TURNS {
        s.turn(format!("question {turn}")).await;
    }
    assert!(s.store.list_context_masks("s1").await.unwrap().is_empty());
}
