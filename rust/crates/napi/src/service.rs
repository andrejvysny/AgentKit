//! The session-service binding: construction, the durable-input surface,
//! event subscription, and shutdown.
//!
//! Inputs arrive as JSON text (the JS wrapper owns camelCase mapping); every
//! response crosses back as JSON text so the portable contract — not a
//! parallel Rust-only schema — defines the wire shape. Shutdown order is
//! explicit: reject outstanding callbacks, stop the worker, drain queued
//! store writes, release the store clone. A dropped JS wrapper never runs
//! any of this, and that is by design: GC must never cancel durable work.

use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentkit_core::ToolRegistry;
use agentkit_host::{
    AgentHost, AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND, ChatExecutor,
    ChatExecutorOptions, HostOptions, HostResult, RunHandle, RunUpdate, SharedRuntime,
    TaskExecutor, WorkerHandle,
};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::errors;
use crate::jsbridge::JsBridge;
use crate::provider::JsProvider;
use crate::tool::JsToolContributor;

const MAX_POLL_BATCH: i32 = 64;
const DEFAULT_POLL_WAIT_MS: u64 = 25;

struct Core {
    host: AgentHost,
    service: AgentSessionService,
    contributor: Arc<JsToolContributor>,
    worker: Option<WorkerHandle>,
    subscriptions: Mutex<Vec<Arc<Subscription>>>,
}

#[napi(js_name = "SessionService")]
pub struct SessionService {
    core: Mutex<Option<Core>>,
    bridge: Arc<JsBridge>,
}

#[napi]
impl SessionService {
    /// Build the full native runtime over a native store: host + chat
    /// executor + session service + worker. Recovery runs before the worker
    /// claims any task, so restart reconciliation always precedes new
    /// modifying work. Exposed on the JS side as `NativeStore.createService`.
    pub(crate) async fn build(
        store_handle: agentkit_store::SqliteStore,
        on_event: ThreadsafeFunction<String>,
    ) -> Result<Self> {
        let bridge = Arc::new(JsBridge::new(on_event));
        let runtime: SharedRuntime = std::sync::Arc::new(agentkit_host::SystemRuntime);
        let contributor = Arc::new(JsToolContributor::new(Arc::clone(&bridge)));
        let executor: Arc<dyn TaskExecutor> = Arc::new(
            ChatExecutor::new(
                Arc::new(JsProvider::new(Arc::clone(&bridge))),
                Arc::new(ToolRegistry::new()),
            )
            .with_options(ChatExecutorOptions {
                tool_contributors: vec![
                    Arc::clone(&contributor) as Arc<dyn agentkit_host::ToolContributor>
                ],
                ..Default::default()
            }),
        );
        let host = AgentHost::new(
            store_handle.clone(),
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
            runtime.clone(),
            HostOptions::default(),
        )
        .map_err(errors::host)?;
        let service = AgentSessionService::new(AgentSessionServiceDeps {
            store: store_handle,
            runtime,
            plan_approval: None,
            wake: Some(host.wake_handle()),
        });
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        // Crash-reconcile interrupted turns BEFORE the worker can claim work.
        host.recover().await.map_err(errors::host)?;
        let worker = host.start_worker();
        Ok(Self {
            core: Mutex::new(Some(Core {
                host,
                service,
                contributor,
                worker: Some(worker),
                subscriptions: Mutex::new(Vec::new()),
            })),
            bridge,
        })
    }

    /// Register a JS-executed tool. `definitionJson` is the portable tool
    /// definition: `{name, version?, effect?, capability?, description?,
    /// inputSchema, outputSchema?, timeoutMs?}`.
    #[napi]
    pub async fn register_tool(&self, definition_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.contributor
            .register(&definition_json)
            .map_err(errors::invalid)
    }

    #[napi]
    pub async fn create_session(&self, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_create_session(&input_json)?;
        let state = core
            .service
            .create_session(&input)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn send(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_send_input(&input_json, WhenBusy::Reject)?;
        let result = core
            .service
            .send(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn follow_up(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_follow_up(&input_json)?;
        let result = core
            .service
            .follow_up(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn steer(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_steer(&input_json)?;
        let result = core
            .service
            .steer(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn get_state(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let state = core
            .service
            .get_state(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn get_execution(&self, session_id: String) -> Result<Option<String>> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let execution = core
            .service
            .get_execution(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(execution.map(|value| value.to_string()))
    }

    #[napi]
    pub async fn list_turns(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let turns = core
            .service
            .list_turns(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(Value::Array(turns).to_string())
    }

    /// The full durable projection of one session (turns, messages, inputs,
    /// receipts) — the reopened-history view.
    #[napi]
    pub async fn export_session(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let exported = core
            .service
            .export_session(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(exported.to_string())
    }

    #[napi]
    pub async fn get_usage(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let usage = core
            .service
            .get_usage(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(usage.to_string())
    }

    /// Answer a pending human request. `inputJson` is
    /// `{requestId, answerId, response}`.
    #[napi]
    pub async fn answer(&self, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_answer(&input_json)?;
        let receipt = core.service.answer(&input).await.map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn approve(
        &self,
        request_id: String,
        answer_id: String,
        note: Option<String>,
        remember: Option<bool>,
    ) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let receipt = core
            .service
            .approve(&request_id, &answer_id, note.as_deref(), remember)
            .await
            .map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn deny(
        &self,
        request_id: String,
        answer_id: String,
        note: Option<String>,
    ) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let receipt = core
            .service
            .deny(&request_id, &answer_id, note.as_deref())
            .await
            .map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn cancel(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.service
            .cancel(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn interrupt(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.service
            .interrupt(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn resume(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.service
            .resume(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn flush_queue(&self, session_id: String) -> Result<Option<String>> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let result = core
            .service
            .flush_queue(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(result.map(send_result_json))
    }

    /// Subscribe to the durable committed event stream of one turn task.
    /// Events are handed out in `seq` order; a gap is impossible by
    /// construction because every poll continues from the last delivered
    /// cursor and each event's `seq` is returned to the caller.
    #[napi]
    pub async fn subscribe_turn(
        &self,
        task_id: String,
        after_seq: Option<f64>,
    ) -> Result<SubscriptionHandle> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let after = match after_seq {
            None => None,
            Some(value) if value.is_finite() && value >= 0.0 && value.fract() == 0.0 => {
                Some(value as u64)
            }
            Some(_) => return Err(errors::invalid("afterSeq must be a non-negative integer")),
        };
        let run: RunHandle = core.host.run_handle(&task_id);
        let receiver = run.subscribe(after);
        let subscription = Arc::new(Subscription {
            receiver: Mutex::new(Some(receiver)),
            closed: AtomicBool::new(false),
        });
        core.subscriptions
            .lock()
            .await
            .push(Arc::clone(&subscription));
        Ok(SubscriptionHandle {
            subscription,
            buffer: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Release the event-loop reference held by the callback delivery
    /// function. The JS wrapper calls this first in `close()`; after it, no
    /// new callback can wake the loop, so a process whose script has ended
    /// can actually exit even while the class object is still referenced.
    #[napi]
    pub fn detach(&self, env: Env) -> Result<()> {
        self.bridge
            .detach(&env)
            .map_err(|message| errors::internal(&message))
    }

    /// Reject every outstanding callback, stop and drain the worker, release
    /// the store clone. Idempotent: a second close is a no-op.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let mut core = self
            .core
            .lock()
            .await
            .take()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        self.bridge.reject_all();
        for subscription in core.subscriptions.lock().await.drain(..) {
            subscription.cancel();
        }
        if let Some(worker) = core.worker.take() {
            worker.shutdown().await.map_err(errors::host)?;
        }
        drop(core);
        Ok(())
    }
}

/// One turn subscription. `poll` drains the next committed events; the
/// stream is pull-based, so a slow consumer applies backpressure to the
/// follower instead of overflowing a queue.
#[napi(js_name = "SubscriptionHandle")]
pub struct SubscriptionHandle {
    subscription: Arc<Subscription>,
    buffer: std::sync::Mutex<Vec<Value>>,
}

struct Subscription {
    receiver: Mutex<Option<mpsc::Receiver<HostResult<RunUpdate>>>>,
    closed: AtomicBool,
}

impl Subscription {
    fn cancel(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.receiver.try_lock() {
            let _ = guard.take();
        }
    }
}

#[napi]
impl SubscriptionHandle {
    /// Drain up to `limit` (max 64) committed events, waiting up to
    /// `waitMs` for at least one when idle. `done` means the turn settled
    /// (or failed/closed) and no further events will arrive.
    #[napi]
    pub async fn poll(&self, limit: Option<i32>, wait_ms: Option<f64>) -> Result<String> {
        let limit = limit.unwrap_or(MAX_POLL_BATCH);
        if !(1..=MAX_POLL_BATCH).contains(&limit) {
            return Err(errors::invalid("limit must be between 1 and 64"));
        }
        let wait = wait_ms
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(DEFAULT_POLL_WAIT_MS as f64)
            .min(30_000.0) as u64;
        let events = std::mem::take(
            self.buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .deref_mut(),
        );
        if events.len() >= usize::try_from(limit).unwrap_or(1) {
            self.finish_batch(events)
        } else {
            self.poll_more(limit, wait, events).await
        }
    }

    /// Stop the subscription. Idempotent; polling afterwards reports
    /// `done` with no events.
    #[napi]
    pub fn cancel(&self) {
        self.subscription.cancel();
    }

    #[napi(getter)]
    pub fn done(&self) -> bool {
        self.subscription.closed.load(Ordering::SeqCst)
    }
}

impl SubscriptionHandle {
    fn finish_batch(&self, mut events: Vec<Value>) -> Result<String> {
        let done = self.subscription.closed.load(Ordering::SeqCst);
        let limit = events.len();
        if !done && limit > usize::try_from(MAX_POLL_BATCH).unwrap_or(64) {
            // Cannot happen through `poll`, but the contract holds: excess
            // stays buffered for the next call.
            let keep = events.split_off(usize::try_from(MAX_POLL_BATCH).unwrap_or(64));
            *self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = keep;
        }
        Ok(json!({ "events": Value::Array(events), "done": done }).to_string())
    }

    async fn poll_more(&self, limit: i32, wait_ms: u64, mut events: Vec<Value>) -> Result<String> {
        let cap = usize::try_from(limit).unwrap_or(1);
        let mut guard = self.subscription.receiver.lock().await;
        let Some(receiver) = guard.as_mut() else {
            self.subscription.closed.store(true, Ordering::SeqCst);
            drop(guard);
            return self.finish_batch(events);
        };
        loop {
            if events.len() >= cap {
                break;
            }
            match receiver.try_recv() {
                Ok(update) => match drain_update(update)? {
                    Drain::Event(value) => events.push(value),
                    Drain::Done(value) => {
                        if let Some(value) = value {
                            events.push(value);
                        }
                        self.subscription.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                },
                Err(_) => break,
            }
        }
        if events.is_empty() && !self.subscription.closed.load(Ordering::SeqCst) {
            // Wait for the first item so idle turns do not spin the JS side.
            match tokio::time::timeout(std::time::Duration::from_millis(wait_ms), receiver.recv())
                .await
            {
                Ok(Some(update)) => match drain_update(update)? {
                    Drain::Event(value) => events.push(value),
                    Drain::Done(value) => {
                        if let Some(value) = value {
                            events.push(value);
                        }
                        self.subscription.closed.store(true, Ordering::SeqCst);
                    }
                },
                Ok(None) => {
                    self.subscription.closed.store(true, Ordering::SeqCst);
                }
                Err(_elapsed) => {}
            }
        }
        drop(guard);
        self.finish_batch(events)
    }
}

enum Drain {
    Event(Value),
    Done(Option<Value>),
}

fn drain_update(update: HostResult<RunUpdate>) -> Result<Drain> {
    match update {
        Ok(RunUpdate::Event(record)) => {
            let value = serde_json::to_value(&record)
                .map_err(|error| errors::internal(&error.to_string()))?;
            Ok(Drain::Event(json!({ "type": "event", "event": value })))
        }
        Ok(RunUpdate::Settled(task)) => {
            let value = serde_json::to_value(&task)
                .map_err(|error| errors::internal(&error.to_string()))?;
            Ok(Drain::Done(Some(
                json!({ "type": "settled", "task": value }),
            )))
        }
        Err(error) => Err(errors::host(error)),
    }
}

// ---------------------------------------------------------------------------
// JSON input mapping (the JS wrapper owns the public camelCase surface; the
// binding parses here so bad input fails with `invalid_input`, not a panic)
// ---------------------------------------------------------------------------

use agentkit_host::{
    AgentSendResult, AnswerInput, CreateSessionInput, FollowUpInput, OnSettled, SendInput,
    SteerInput, WhenBusy,
};

fn parse_object(text: &str, what: &str) -> Result<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.is_object())
        .ok_or_else(|| errors::invalid(format!("{what} must be a JSON object")))
}

fn optional_string(object: &Value, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(errors::invalid(format!(
            "{key} must be a string when present"
        ))),
    }
}

fn parse_when_busy(object: &Value) -> Result<WhenBusy> {
    match object.get("whenBusy") {
        None | Some(Value::Null) => Ok(WhenBusy::Reject),
        Some(Value::String(value)) => match value.as_str() {
            "reject" => Ok(WhenBusy::Reject),
            "queue" => Ok(WhenBusy::Queue),
            "steer" => Ok(WhenBusy::Steer),
            other => Err(errors::invalid(format!("unknown whenBusy \"{other}\""))),
        },
        Some(_) => Err(errors::invalid("whenBusy must be a string")),
    }
}

fn parse_on_settled(object: &Value, key: &str) -> Result<Option<OnSettled>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => match value.as_str() {
            "followUp" | "follow-up" => Ok(Some(OnSettled::FollowUp)),
            "cancel" => Ok(Some(OnSettled::Cancel)),
            other => Err(errors::invalid(format!("unknown {key} \"{other}\""))),
        },
        Some(_) => Err(errors::invalid(format!("{key} must be a string"))),
    }
}

fn required_content(object: &Value) -> Result<Value> {
    object
        .get("content")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| errors::invalid("content is required"))
}

fn parse_create_session(text: &str) -> Result<CreateSessionInput> {
    let object = parse_object(text, "the session input")?;
    let profile_id = optional_string(&object, "profileId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("profileId is required and must be a non-empty string"))?;
    let skills = match object.get("skills") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(items.clone()),
        Some(_) => return Err(errors::invalid("skills must be an array when present")),
    };
    let metadata = match object.get("metadata") {
        None | Some(Value::Null) => None,
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err(errors::invalid("metadata must be an object when present")),
    };
    Ok(CreateSessionInput {
        session_id: optional_string(&object, "sessionId")?,
        title: optional_string(&object, "title")?,
        profile_id,
        profile_digest: optional_string(&object, "profileDigest")?,
        provider_id: optional_string(&object, "providerId")?,
        model: optional_string(&object, "model")?,
        workspace: object.get("workspace").cloned().filter(|v| !v.is_null()),
        context: object.get("context").cloned().filter(|v| !v.is_null()),
        skills,
        turn_kind: optional_string(&object, "turnKind")?,
        execution_mode: optional_string(&object, "executionMode")?,
        metadata,
    })
}

fn parse_send_input(text: &str, default_busy: WhenBusy) -> Result<SendInput> {
    let object = parse_object(text, "the send input")?;
    let when_busy = match object.get("whenBusy") {
        None | Some(Value::Null) => default_busy,
        Some(_) => parse_when_busy(&object)?,
    };
    Ok(SendInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
        when_busy,
        turn_id: optional_string(&object, "turnId")?,
        on_settled: parse_on_settled(&object, "onSettled")?,
    })
}

fn parse_follow_up(text: &str) -> Result<FollowUpInput> {
    let object = parse_object(text, "the follow-up input")?;
    Ok(FollowUpInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
    })
}

fn parse_steer(text: &str) -> Result<SteerInput> {
    let object = parse_object(text, "the steer input")?;
    Ok(SteerInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
        turn_id: optional_string(&object, "turnId")?,
        on_settled: parse_on_settled(&object, "onSettled")?,
    })
}

fn parse_answer(text: &str) -> Result<AnswerInput> {
    let object = parse_object(text, "the answer input")?;
    let request_id = optional_string(&object, "requestId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("requestId is required"))?;
    let answer_id = optional_string(&object, "answerId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("answerId is required"))?;
    let response = object
        .get("response")
        .cloned()
        .ok_or_else(|| errors::invalid("response is required"))?;
    Ok(AnswerInput {
        request_id,
        answer_id,
        response,
    })
}

fn session_state_json(state: &agentkit_host::AgentSessionState) -> String {
    let pending: Vec<Value> = state
        .pending
        .iter()
        .filter_map(|record| serde_json::to_value(record).ok())
        .collect();
    let waiting = state
        .waiting
        .as_ref()
        .and_then(|record| serde_json::to_value(record).ok());
    json!({
        "sessionId": state.session_id,
        "descriptor": state.descriptor,
        "activeTurn": state.active_turn,
        "pending": pending,
        "waiting": waiting,
        "waitingApproval": state.waiting_approval,
        "waitingPlan": state.waiting_plan,
        "execution": state.execution,
    })
    .to_string()
}

fn send_result_json(result: AgentSendResult) -> String {
    match result {
        AgentSendResult::Turn {
            turn_id,
            user_message_id,
            assistant_message_id,
        } => json!({
            "kind": "turn",
            "turnId": turn_id,
            "userMessageId": user_message_id,
            "assistantMessageId": assistant_message_id,
        })
        .to_string(),
        AgentSendResult::Queued { input } => {
            let record = serde_json::to_value(&*input).unwrap_or(Value::Null);
            json!({ "kind": "queued", "input": record }).to_string()
        }
    }
}

fn receipt_json(receipt: &agentkit_store::AnswerReceipt) -> String {
    json!({
        "requestId": receipt.request_id,
        "answerId": receipt.answer_id,
        "turnId": receipt.turn_id,
        "messageId": receipt.message_id,
        "duplicate": receipt.duplicate,
    })
    .to_string()
}
