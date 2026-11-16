//! Roadmap 2.5, compaction for every session: a chat that used to overflow
//! compacts and keeps its originals (CE-14); the provider-continuation lane
//! compacts too, voiding its checkpoint and starting a new one (CE-15); a
//! compaction adopts pending project rules under ONE cache break; token
//! calibration learns from the provider's counts; disabling masks is reported
//! once.
#![cfg(unix)]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, ConnectionIdentity, CoreError, ProviderEvent, ProviderStream,
    ToolRegistry,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const WINDOW: &str = r#"{"windowTokens":4096,"reserveTokens":1024,"keepRecentTokens":1}"#;

/// Answers every request; a summary request (its text carries
/// `<conversation>`) gets a fixed summary. With `responses` it poses as a
/// Responses connection and saves a checkpoint after each main request, the
/// way the real provider does through the request's continuation store.
struct Probe {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    responses: bool,
    prompt_tokens: Option<u64>,
    /// Fails the first main request that follows a summary request: the
    /// attempt dies after the compaction stored its summary. 0 = off,
    /// 1 = armed, 2 = a summary was seen, 3 = failed.
    fail_after_summary: AtomicU8,
}

#[async_trait]
impl ChatProvider for Probe {
    fn id(&self) -> &str {
        "test"
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        self.responses.then(|| ConnectionIdentity {
            provider_id: "test".into(),
            protocol: "responses".into(),
            connection_id: "connection".into(),
            generation: 1,
        })
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let summary = input
            .messages
            .iter()
            .any(|message| message.content.to_string().contains("<conversation>"));
        let (scope, store) = (
            input.continuation_scope.clone(),
            input.continuation_store.clone(),
        );
        let (model, count) = (input.model.clone(), input.messages.len());
        self.requests.lock().unwrap().push(input);
        let stage = &self.fail_after_summary;
        if summary {
            let _ = stage.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
        } else if stage
            .compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Box::pin(futures::stream::once(async {
                Err::<ProviderEvent, _>(CoreError::responses("injected_failure"))
            }));
        }
        let usage = self
            .prompt_tokens
            .filter(|_| !summary)
            .map(|tokens| {
                ProviderEvent::Usage(json!({
                    "callId":"call","attempt":1,"step":0,"model":"model","source":"response",
                    "finalForCall":true,"input_uncached":tokens,"cache_read":0
                }))
            })
            .into_iter()
            .map(Ok);
        let done = ProviderEvent::Completed {
            content: if summary {
                "summary of earlier turns"
            } else {
                "answer"
            }
            .into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        };
        let head = futures::stream::iter(usage);
        if let (Some(store), Some(scope), false) = (store, scope, summary) {
            let state = json!({
                "version": 1,
                "scope": {"providerId":"test","protocol":"responses","connectionId":"connection",
                    "generation":1,"chatId":scope.chat_id,"branchId":scope.branch_id,"model":model},
                "messageCount": count,
                "messageDigest": "0".repeat(64),
                "inputItems": [],
            });
            return Box::pin(head.chain(futures::stream::once(async move {
                store.save(state).await?;
                Ok::<_, CoreError>(done)
            })));
        }
        Box::pin(head.chain(futures::stream::once(async move { Ok(done) })))
    }
}

#[derive(Default)]
struct Setup {
    auto_compaction: bool,
    responses: bool,
    prompt_tokens: Option<u64>,
    fail_after_summary: bool,
    workspace: Option<PathBuf>,
    context: Option<Value>,
}

struct Harness {
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    store: SqliteStore,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Harness {
    async fn start(setup: Setup) -> Self {
        let store = SqliteStore::memory(64).expect("store");
        let requests = Arc::new(Mutex::new(vec![]));
        let provider = Probe {
            requests: requests.clone(),
            responses: setup.responses,
            prompt_tokens: setup.prompt_tokens,
            fail_after_summary: AtomicU8::new(u8::from(setup.fail_after_summary)),
        };
        let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
            ChatExecutor::new(Arc::new(provider), Arc::new(ToolRegistry::new())).with_options(
                ChatExecutorOptions {
                    auto_compaction: setup.auto_compaction,
                    project_instructions: setup
                        .workspace
                        .as_ref()
                        .map(|_| Arc::new(FileProjectInstructions) as _),
                    ..Default::default()
                },
            ),
        );
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
        });
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        service
            .create_session(&CreateSessionInput {
                session_id: Some("s1".into()),
                title: None,
                // The `chat` profile switches project rules off; a session with a
                // workspace is here for its rules.
                profile_id: if setup.workspace.is_some() {
                    "coding"
                } else {
                    "chat"
                }
                .into(),
                profile_digest: None,
                reasoning_effort: None,
                content_selection: None,
                provider_id: None,
                model: Some("model".into()),
                workspace: setup
                    .workspace
                    .as_ref()
                    .map(|root| json!({"root": root.display().to_string(), "trusted": true})),
                context: None,
                skills: None,
                turn_kind: None,
                execution_mode: None,
                metadata: None,
            })
            .await
            .unwrap();
        let harness = Self {
            host,
            service,
            store,
            requests,
        };
        if let Some(context) = setup.context {
            harness.configure(context).await;
        }
        harness
    }

    async fn configure(&self, context: Value) {
        self.service
            .configure("s1", None, Some(context), None)
            .await
            .unwrap();
    }

    /// One turn, run to its end; its id.
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
            panic!("an idle session starts the turn");
        };
        self.host.run_once().await.unwrap();
        turn_id
    }

    async fn status(&self, turn_id: &str) -> TaskStatus {
        self.store.get_task(turn_id).await.unwrap().unwrap().status
    }

    async fn events(&self, turn_id: &str, kind: &str) -> Vec<Value> {
        self.store
            .events(turn_id, None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.event["type"] == kind)
            .map(|event| event.event["data"].clone())
            .collect()
    }

    /// The requests the session's own turns sent (no summarizer calls).
    fn mains(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                !request
                    .messages
                    .iter()
                    .any(|message| message.content.to_string().contains("<conversation>"))
            })
            .cloned()
            .collect()
    }

    async fn metadata(&self) -> Value {
        self.store.get_chat("s1").await.unwrap().unwrap().metadata
    }
}

fn window(extra: Value) -> Value {
    let mut context: Value = serde_json::from_str(WINDOW).unwrap();
    for (key, value) in extra.as_object().unwrap() {
        context[key] = value.clone();
    }
    context
}

fn system_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.to_string())
        .collect()
}

fn contains(request: &ChatRequest, needle: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| message.content.to_string().contains(needle))
}

/// CE-14: the chat that used to end in `context_overflow` compacts, carries on
/// from the summary, and every original stays readable in the transcript.
#[tokio::test]
async fn a_chat_that_overflowed_continues_with_a_summary_and_keeps_its_originals() {
    let chat = Harness::start(Setup {
        context: Some(window(json!({"autoCompaction": true}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(7000)).await;
    let turn = chat.turn("b".repeat(7000)).await;
    assert_eq!(chat.status(&turn).await, TaskStatus::Completed);
    let summaries = chat.store.list_context_summaries("s1").await.unwrap();
    assert_eq!(summaries.len(), 1);
    let compacted = chat.events(&turn, "run.context.compacted").await;
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0]["summaryId"], summaries[0]["summaryId"]);
    assert_eq!(
        compacted[0]["coversThroughMessageId"],
        summaries[0]["coversThroughMessageId"]
    );
    assert_eq!(
        compacted[0]["firstKeptMessageId"],
        summaries[0]["firstKeptMessageId"]
    );
    let last = chat.mains().pop().unwrap();
    assert!(contains(&last, "<summary>"));
    assert!(!contains(&last, &"a".repeat(7000)));
    let stored = chat.store.messages("s1", false).await.unwrap();
    for original in ["a".repeat(7000), "b".repeat(7000)] {
        assert!(
            stored
                .iter()
                .any(|message| message.content.to_string().contains(&original)),
            "the original stays in the transcript"
        );
    }
    assert_eq!(chat.events(&turn, "context.cache_break").await.len(), 1);
}

/// The same chat with the session's switch off keeps the old refusal, so the
/// override works in both directions.
#[tokio::test]
async fn the_session_switch_overrides_the_executor_default_both_ways() {
    let off = Harness::start(Setup {
        auto_compaction: true,
        context: Some(window(json!({"autoCompaction": false}))),
        ..Setup::default()
    })
    .await;
    off.turn("a".repeat(7000)).await;
    let turn = off.turn("b".repeat(7000)).await;
    assert_ne!(off.status(&turn).await, TaskStatus::Completed);
    assert!(
        off.store
            .list_context_summaries("s1")
            .await
            .unwrap()
            .is_empty()
    );
}

/// CE-15: a Responses session masks and compacts like any other. The old
/// checkpoint is discarded, the request replays the compacted history in
/// full, and the provider's new checkpoint is what the next turn resumes from.
#[tokio::test]
async fn a_responses_session_compacts_and_resumes_from_a_new_checkpoint() {
    let chat = Harness::start(Setup {
        auto_compaction: true,
        responses: true,
        context: Some(window(json!({}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(7000)).await;
    let before = chat.mains();
    assert!(before[0].continuation.is_none(), "the first turn has none");
    let turn = chat.turn("b".repeat(7000)).await;
    assert_eq!(chat.status(&turn).await, TaskStatus::Completed);
    assert_eq!(
        chat.store.list_context_summaries("s1").await.unwrap().len(),
        1
    );
    assert!(
        chat.events(&turn, "run.warning")
            .await
            .iter()
            .all(|warning| warning["code"] != "compaction_failed")
    );
    let compacted = chat.mains().pop().unwrap();
    assert!(contains(&compacted, "<summary>"));
    assert!(
        compacted.continuation.is_none() && !compacted.continuation_required,
        "the stale checkpoint is not sent"
    );
    assert!(compacted.continuation_scope.is_some());
    let breaks = chat.events(&turn, "context.cache_break").await;
    assert_eq!(breaks.len(), 1);
    assert_eq!(breaks[0]["reason"], "compact");
    assert_eq!(breaks[0]["checkpoint_reset"], true);
    // The next turn resumes from the checkpoint the compacted turn saved.
    let next = chat.turn("c".into()).await;
    assert_eq!(chat.status(&next).await, TaskStatus::Completed);
    assert_eq!(
        chat.store.list_context_summaries("s1").await.unwrap().len(),
        1
    );
    let resumed = chat.mains().pop().unwrap();
    assert!(resumed.continuation.is_some() && resumed.continuation_required);
    assert!(contains(&resumed, "<summary>"));
    assert!(chat.events(&next, "context.cache_break").await.is_empty());
}

fn rules(ten: &str) -> String {
    (1..=20)
        .map(|n| {
            if n == 10 {
                format!("{ten}\n")
            } else {
                format!("rule {n}\n")
            }
        })
        .collect()
}

/// A compaction adopts the rules edited since the freeze, moves the prefix to
/// a new revision, and reports ONE cache break; the turn after it finds
/// nothing left to report.
#[tokio::test]
async fn a_compaction_adopts_pending_rules_under_one_cache_break() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), rules("rule 10")).unwrap();
    let chat = Harness::start(Setup {
        auto_compaction: true,
        workspace: Some(dir.path().to_path_buf()),
        context: Some(window(json!({}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(7000)).await;
    std::fs::write(dir.path().join("AGENTS.md"), rules("rule ten edited")).unwrap();
    let turn = chat.turn("b".repeat(7000)).await;
    assert_eq!(chat.status(&turn).await, TaskStatus::Completed);
    let breaks = chat.events(&turn, "context.cache_break").await;
    assert_eq!(breaks.len(), 1, "{breaks:?}");
    assert_eq!(breaks[0]["reason"], "compact");
    assert_eq!(breaks[0]["rules_refrozen"], true);
    let compacted = chat.mains().pop().unwrap();
    assert!(system_text(&compacted).contains("rule ten edited"));
    let prefix = read_session_prefix(&chat.metadata().await).expect("prefix");
    assert_eq!(prefix.revision, 2.0);
    assert_eq!(breaks[0]["revision"], 2);
    let next = chat.turn("c".into()).await;
    assert!(chat.events(&next, "context.cache_break").await.is_empty());
    assert!(system_text(&chat.mains().pop().unwrap()).contains("rule ten edited"));
    assert_eq!(
        read_session_prefix(&chat.metadata().await)
            .unwrap()
            .revision,
        2.0
    );
}

/// The estimate error is learned from what the provider reports: rho moves a
/// 0.3 step toward actual / estimated per request and is kept per model.
#[tokio::test]
async fn rho_is_learned_from_reported_prompt_tokens_and_stored_per_model() {
    let chat = Harness::start(Setup {
        prompt_tokens: Some(2000),
        context: Some(window(json!({}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(4000)).await;
    let entry = chat.metadata().await["agentkit.session.tokenCalibration"]["model"].clone();
    let rho = entry["rho"].as_f64().expect("rho is stored");
    // About 1000 estimated tokens against 2000 reported: 0.7 + 0.3 * ~2.
    assert!((1.19..=1.301).contains(&rho), "rho = {rho}");
    assert_eq!(entry["samples"], 1);
    chat.turn("b".into()).await;
    let second = chat.metadata().await["agentkit.session.tokenCalibration"]["model"].clone();
    assert_eq!(second["samples"], 2);
    assert!(second["rho"].as_f64().unwrap() > rho);
}

/// Switching masking off on a session that holds masks changes its prefix:
/// that is reported once, not on every turn.
#[tokio::test]
async fn disabling_masking_reports_the_invalidated_masks_once() {
    let chat = Harness::start(Setup {
        context: Some(window(json!({"masking": {"enabled": false}}))),
        ..Setup::default()
    })
    .await;
    chat.store
        .save_context_mask(json!({
            "maskId":"mask-1","sessionId":"s1","sourceHash":"hash","createdAt":"2026-01-01T00:00:00.000Z",
            "tokensBefore":10,"tokensAfter":5,"coveredCallIds":[],"masks":[],"images":[]
        }))
        .await
        .unwrap();
    let first = chat.turn("one".into()).await;
    let breaks = chat.events(&first, "context.cache_break").await;
    assert_eq!(breaks.len(), 1, "{breaks:?}");
    assert_eq!(breaks[0]["reason"], "mask_invalidated");
    let second = chat.turn("two".into()).await;
    assert!(chat.events(&second, "context.cache_break").await.is_empty());
}

/// A checkpoint is valid for the generation it was saved under, not for the
/// turn that voided its predecessor: an attempt that died after the compaction
/// stored its summary leaves the next one on a history the old checkpoint does
/// not describe, so that one is not resumed either.
#[tokio::test]
async fn an_attempt_that_failed_after_the_summary_was_stored_does_not_resume_the_old_checkpoint() {
    let chat = Harness::start(Setup {
        auto_compaction: true,
        responses: true,
        fail_after_summary: true,
        context: Some(window(json!({}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(7000)).await;
    chat.turn("b".repeat(7000)).await;
    assert_eq!(
        chat.store.list_context_summaries("s1").await.unwrap().len(),
        1,
        "the summary outlived the failed attempt"
    );
    if chat.mains().len() < 3 {
        chat.turn("c".into()).await;
    }
    let mains = chat.mains();
    assert!(mains[0].continuation.is_none());
    assert!(mains[1].continuation.is_none(), "the failing request");
    assert!(
        mains[2].continuation.is_none() && !mains[2].continuation_required,
        "the checkpoint of the first turn describes the history before the summary"
    );
}

/// INV-3 is derived from stored state: a rewrite whose event was lost (the
/// epoch announced is behind the masks and summary that apply) is announced by
/// the next attempt, once.
#[tokio::test]
async fn a_lost_cache_break_is_announced_by_the_next_attempt_once() {
    let chat = Harness::start(Setup {
        auto_compaction: true,
        context: Some(window(json!({}))),
        ..Setup::default()
    })
    .await;
    chat.turn("a".repeat(7000)).await;
    let turn = chat.turn("b".repeat(7000)).await;
    assert_eq!(chat.events(&turn, "context.cache_break").await.len(), 1);
    let mut metadata = chat.metadata().await;
    metadata
        .as_object_mut()
        .unwrap()
        .remove("agentkit.session.cacheBreakEpoch");
    chat.store
        .update_chat("s1", None, Some(metadata), None, 1000)
        .await
        .unwrap();
    let next = chat.turn("c".into()).await;
    let breaks = chat.events(&next, "context.cache_break").await;
    assert_eq!(breaks.len(), 1, "{breaks:?}");
    assert_eq!(breaks[0]["reason"], "compact");
    assert_eq!(breaks[0]["recovered"], true);
    let after = chat.turn("d".into()).await;
    assert!(chat.events(&after, "context.cache_break").await.is_empty());
}
