use crate::{
    budget::{minimum, timeout},
    tool_execution, ChatMessage, ChatProvider, ChatRequest, CoreError, ProviderEvent, Runtime,
    SystemRuntime, ToolCall, ToolLimits, ToolRegistry,
};
use agentkit_contracts::AiRunEvent;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
#[path = "run_cleanup.rs"]
mod cleanup;

#[derive(Clone)]
pub struct RunOptions {
    pub request: ChatRequest,
    pub run_id: Option<String>,
    pub first_seq: u64,
    pub attempt_id: Option<String>,
    pub max_tool_iterations: u32,
    pub max_tool_calls_per_iteration: usize,
    pub default_tool_timeout_ms: Option<u64>,
    pub limits: ToolLimits,
    pub chat_id: Option<String>,
    pub scope_id: Option<String>,
    pub user_id: Option<String>,
    pub bindings: Vec<Value>,
    pub cancellation: CancellationToken,
    pub runtime: Arc<dyn Runtime>,
    pub event_sink: Option<Arc<dyn crate::RunEventSink>>,
    pub dispatch_guard: Option<Arc<dyn crate::DispatchGuard>>,
    /// Host park authority for human requests; absent means tools cannot
    /// ask (they must deny instead of hanging).
    pub suspender: Option<Arc<dyn crate::SuspendSink>>,
    /// Cancelled when the host has durably recorded an interrupt request
    /// for this task; the run stops at the next safe boundary.
    pub interruption: Option<CancellationToken>,
    /// Host steering source (ADR 0016 §3); polled only at tool boundaries.
    pub steering: Option<Arc<dyn crate::SteeringSource>>,
    /// Approved calls that never ran: executed as the first batch, ahead of
    /// any model request (ADR 0019 part 1). Their ids are excluded from
    /// orphan reconciliation by the host.
    pub resume_calls: Vec<ToolCall>,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            request: ChatRequest::default(),
            run_id: None,
            first_seq: 0,
            attempt_id: None,
            max_tool_iterations: 4,
            max_tool_calls_per_iteration: 8,
            default_tool_timeout_ms: None,
            limits: ToolLimits::default(),
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            cancellation: CancellationToken::new(),
            runtime: Arc::new(SystemRuntime),
            event_sink: None,
            dispatch_guard: None,
            suspender: None,
            interruption: None,
            steering: None,
            resume_calls: vec![],
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminal {
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug)]
pub struct RunResult {
    pub run_id: String,
    pub terminal: Terminal,
    pub appended_messages: Vec<ChatMessage>,
    pub iterations: u32,
    pub continuation: Option<Value>,
    /// True when the run ended because a tool parked on a human request;
    /// the host must not settle a parked turn.
    pub suspended: bool,
}
pub struct RunHandle {
    pub events: ReceiverStream<AiRunEvent>,
    pub result: tokio::task::JoinHandle<RunResult>,
}
pub fn run_chat(
    options: RunOptions,
    provider: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
) -> RunHandle {
    let (sender, receiver) = mpsc::channel(64);
    let result = tokio::spawn(async move {
        let mut state = State::new(options, sender);
        let terminal = match state.run(provider, registry).await {
            Ok(terminal) => terminal,
            Err(error) => state.end_error(error).await,
        };
        RunResult {
            run_id: state.run_id,
            terminal,
            appended_messages: state.messages[state.initial_length..].to_vec(),
            iterations: state.iteration,
            continuation: state.continuation,
            suspended: state.suspended,
        }
    });
    RunHandle {
        events: ReceiverStream::new(receiver),
        result,
    }
}
struct State {
    options: RunOptions,
    sender: mpsc::Sender<AiRunEvent>,
    run_id: String,
    seq: u64,
    messages: Vec<ChatMessage>,
    initial_length: usize,
    pending_messages: Vec<ChatMessage>,
    suspended: bool,
    iteration: u32,
    continuation: Option<Value>,
    applied_steers: HashSet<String>,
}
#[derive(Default)]
struct Turn {
    delta: String,
    content: Option<String>,
    calls: Vec<ToolCall>,
    requested: Vec<ToolCall>,
    announced: HashSet<String>,
    finish: String,
    saw_no_tool_warning: bool,
}
impl State {
    fn new(options: RunOptions, sender: mpsc::Sender<AiRunEvent>) -> Self {
        let run_id = options
            .run_id
            .clone()
            .unwrap_or_else(|| options.runtime.id("run_"));
        let messages = options.request.messages.clone();
        let initial_length = messages.len();
        let continuation = options.request.continuation.clone();
        Self {
            run_id,
            seq: options.first_seq,
            messages,
            initial_length,
            pending_messages: Vec::new(),
            suspended: false,
            iteration: 0,
            continuation,
            applied_steers: HashSet::new(),
            options,
            sender,
        }
    }
    async fn emit(&mut self, kind: &str, data: Value) -> Result<(), CoreError> {
        let mut value = json!({"type":kind,"runId":self.run_id,"timestamp":self.options.runtime.timestamp(),
            "contractVersion":agentkit_contracts::CONTRACT_VERSION,"eventId":self.options.runtime.id("evt_"),"seq":self.seq,"data":data});
        if let Some(id) = &self.options.attempt_id {
            value["attemptId"] = id.clone().into();
        }
        let event = serde_json::from_value(value)
            .map_err(|_| CoreError::new("provider_error", "Invalid provider event"))?;
        if let Some(sink) = &self.options.event_sink {
            if let Err(error) = sink.append_messages(&event, &self.pending_messages).await {
                self.options.cancellation.cancel();
                return Err(error);
            }
        }
        self.seq += 1;
        self.pending_messages.clear();
        let _ = self.sender.send(event).await;
        Ok(())
    }
    async fn warning(&mut self, code: &str, message: &str) -> Result<(), CoreError> {
        self.emit("run.warning", json!({"code":code,"message":message}))
            .await
    }
    /// Poll the steering source and place its user messages onto the
    /// conversation (ADR 0016 §3). Never fails the run: a hanging or
    /// cancelled source just means no steer this step — the inputs stay
    /// pending with the host and become follow-ups at settlement. Each
    /// placement is announced BEFORE the message joins the history, so the
    /// log never claims an input the model did not see.
    async fn apply_steering(&mut self, step: u32) -> Result<(), CoreError> {
        let Some(source) = self.options.steering.clone() else {
            return Ok(());
        };
        const HOOK_TIMEOUT: Duration = Duration::from_millis(5000);
        let polled = tokio::select! {
            _ = self.options.cancellation.cancelled() => return Ok(()),
            result = tokio::time::timeout(HOOK_TIMEOUT, source.pending()) => {
                match result {
                    Err(_) => {
                        self.warning(
                            "hook_timeout",
                            "Steering source timed out; no steering applied this step.",
                        )
                        .await?;
                        return Ok(());
                    }
                    Ok(items) => items,
                }
            }
        };
        for item in polled {
            if self.applied_steers.contains(&item.input_id) {
                continue;
            }
            if item.message.role != "user" {
                self.warning(
                    "steering_rejected",
                    &format!(
                        "Steering input {} rejected: only role \"user\" may steer.",
                        item.input_id
                    ),
                )
                .await?;
                continue;
            }
            self.applied_steers.insert(item.input_id.clone());
            self.emit(
                "run.input.applied",
                json!({"inputId": item.input_id, "kind": "steer", "step": step}),
            )
            .await?;
            self.messages.push(item.message);
        }
        Ok(())
    }
    async fn end_error(&mut self, error: CoreError) -> Terminal {
        if error.code == "cancelled" {
            let _ = self
                .emit("run.cancelled", json!({"reason":"aborted"}))
                .await;
            Terminal::Cancelled
        } else {
            let _ = self
                .emit(
                    "run.failed",
                    json!({"errorCode":error.code,"errorMessage":error.message}),
                )
                .await;
            Terminal::Failed
        }
    }
    fn check(&self) -> Result<(), CoreError> {
        if let Some(budget) = &self.options.request.budget {
            budget.check()?;
        }
        if self.options.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        Ok(())
    }
    /// Durable interrupt observed by the host heartbeat: the run stops at
    /// the next safe boundary and the host parks the turn.
    pub(super) fn interrupted(&self) -> bool {
        self.options
            .interruption
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
    }
    async fn run(
        &mut self,
        provider: Arc<dyn ChatProvider>,
        registry: Arc<ToolRegistry>,
    ) -> Result<Terminal, CoreError> {
        self.options.request.tools = registry.definitions();
        if !self.options.resume_calls.is_empty() {
            // Approved calls that never ran execute first, ahead of any
            // model request (ADR 0019 part 1); their assistant record is
            // already in the history the host assembled.
            self.iteration = 1;
            let calls = self.options.resume_calls.clone();
            let cap = calls.len();
            self.tools(&registry, &HashSet::new(), "", calls, cap, false)
                .await?;
            if self.suspended {
                return Ok(Terminal::Completed);
            }
            // The resumed batch is a tool boundary like any other: a steer
            // sent while the call waited for its approval is read before the
            // next model call.
            if self.options.steering.is_some() {
                self.apply_steering(1).await?;
            }
        }
        for iteration in 1..=self.options.max_tool_iterations.max(1) {
            self.iteration = iteration;
            self.check()?;
            if self.interrupted() {
                return Ok(Terminal::Completed);
            }
            let turn = self.provider_turn(provider.clone(), &registry).await?;
            let content = turn.content.clone().unwrap_or(turn.delta.clone());
            let mut calls = if turn.calls.is_empty() {
                turn.requested.clone()
            } else {
                turn.calls.clone()
            };
            let duplicates = tool_execution::dedupe(&mut calls);
            if turn.finish == "length" {
                self.warning(
                    "truncated",
                    "Response truncated (finish_reason=length); increase max output tokens.",
                )
                .await?;
            }
            if !calls.is_empty() {
                if !duplicates.is_empty() {
                    self.warning("duplicate_tool_call_id",&format!("Provider reused tool call id(s) {} in one turn; the later call(s) were re-keyed so each stays answerable.",duplicates.join(", "))).await?;
                }
                self.tools(
                    &registry,
                    &turn.announced,
                    &content,
                    calls,
                    self.options.max_tool_calls_per_iteration,
                    true,
                )
                .await?;
                if self.suspended {
                    // The turn is durably parked; this attempt ends quietly.
                    return Ok(Terminal::Completed);
                }
                // Steers are polled ONLY here: after tools ran and another
                // model call follows. Not before the first request, after a
                // final answer, or on the last iteration; leftovers stay
                // pending and the host turns them into follow-ups at
                // settlement.
                if self.options.steering.is_some() && iteration < self.options.max_tool_iterations {
                    self.apply_steering(iteration + 1).await?;
                }
            } else {
                self.check()?;
                if turn.finish == "tool_calls" && !turn.saw_no_tool_warning {
                    self.warning("tool_call_unparseable","Provider reported finish_reason=tool_calls but emitted no usable tool call.").await?;
                }
                let message = ChatMessage::text("assistant", &content);
                self.pending_messages.push(message.clone());
                self.messages.push(message);
                self.emit("run.completed",json!({"iterations":iteration,"finishReason":if turn.finish.is_empty() { "stop" } else { &turn.finish }})).await?;
                return Ok(Terminal::Completed);
            }
        }
        self.warning(
            "max_iterations",
            &format!(
                "Reached maxToolIterations={} without final answer.",
                self.options.max_tool_iterations.max(1)
            ),
        )
        .await?;
        self.emit(
            "run.completed",
            json!({"iterations":self.iteration,"finishReason":"max_iterations"}),
        )
        .await?;
        Ok(Terminal::Completed)
    }
    async fn provider_turn(
        &mut self,
        provider: Arc<dyn ChatProvider>,
        registry: &ToolRegistry,
    ) -> Result<Turn, CoreError> {
        let mut request = self.options.request.clone();
        request.run_id = self.run_id.clone();
        request.runtime = Some(self.options.runtime.clone());
        request.messages = self.messages.clone();
        request.tools = registry.definitions();
        request.continuation = self.continuation.clone();
        request.continuation_required |= request.continuation.is_some();
        let budget = request.budget.clone();
        if !provider.tracks_transport_requests() {
            if let Some(budget) = &budget {
                budget.consume("providerRequests").await?;
            }
        }
        let usage_cleanup = provider.supports_usage_cleanup();
        let provider_cancellation = self.options.cancellation.child_token();
        let mut stream = provider.stream_chat(request, provider_cancellation.clone());
        let mut turn = Turn::default();
        let mut first = true;
        let mut last_activity = tokio::time::Instant::now();
        loop {
            if let Err(error) = self.check() {
                provider_cancellation.cancel();
                if usage_cleanup {
                    self.flush_usage(&mut stream).await?;
                }
                return Err(error);
            }
            let phase = if first { "first_byte" } else { "stream_idle" };
            let elapsed = last_activity.elapsed().as_millis() as u64;
            let activity_limit = budget
                .as_ref()
                .and_then(|budget| {
                    if first {
                        budget.limits.first_byte_ms
                    } else {
                        budget.limits.stream_idle_ms
                    }
                })
                .map(|limit| limit.saturating_sub(elapsed));
            let overall_limit = budget.as_ref().and_then(|budget| budget.remaining_ms());
            let duration = minimum(activity_limit, overall_limit);
            let phase = if overall_limit
                .is_some_and(|limit| activity_limit.is_none_or(|activity| limit < activity))
            {
                "overall"
            } else {
                phase
            };
            let next = tokio::select! { biased;
                _ = self.options.cancellation.cancelled() => Err(CoreError::new("cancelled","aborted")),
                value = async { match duration { Some(ms) => tokio::time::timeout(Duration::from_millis(ms),stream.next()).await.map_err(|_|timeout(phase)),None => Ok(stream.next().await) } } => value,
            };
            let next = match next {
                Ok(value) => value,
                Err(error) => {
                    provider_cancellation.cancel();
                    if usage_cleanup {
                        self.flush_usage(&mut stream).await?;
                    }
                    return Err(error);
                }
            };
            let Some(event) = next else {
                break;
            };
            let event = event?;
            if matches!(
                &event,
                ProviderEvent::Delta(_)
                    | ProviderEvent::Completed { .. }
                    | ProviderEvent::ToolRequested(_)
                    | ProviderEvent::Usage(_)
                    | ProviderEvent::Activity
            ) || matches!(&event, ProviderEvent::Event {kind,..} if kind == "run.message.completed")
            {
                first = false;
                last_activity = tokio::time::Instant::now();
            }
            self.provider_event(&mut turn, event).await?;
        }
        if budget.is_some() && turn.content.is_none() {
            return Err(CoreError::new(
                "incomplete_stream",
                "Provider stream ended without a completed response",
            ));
        }
        Ok(turn)
    }
    async fn provider_event(
        &mut self,
        turn: &mut Turn,
        event: ProviderEvent,
    ) -> Result<(), CoreError> {
        match event {
            ProviderEvent::Started => {
                if self.iteration == 1 {
                    self.emit("run.started",json!({"model":self.options.request.model,"toolCount":self.options.request.tools.len()})).await?;
                }
            }
            ProviderEvent::Delta(delta) => {
                turn.delta.push_str(&delta);
                self.emit("run.message.delta", json!({"delta":delta}))
                    .await?;
            }
            ProviderEvent::Completed {
                content,
                tool_calls,
                finish_reason,
            } => {
                self.emit("run.message.completed",json!({"content":content,"toolCallCount":tool_calls.len(),"toolCalls":tool_calls,"finishReason":finish_reason})).await?;
                turn.content = Some(content);
                turn.calls = tool_calls;
                turn.finish = finish_reason;
            }
            ProviderEvent::ToolRequested(call) => {
                turn.announced.insert(call.id.clone());
                turn.requested.push(call.clone());
                self.emit("run.tool.requested",json!({"toolCallId":call.id,"toolName":call.name,"argumentsJson":call.arguments_json})).await?;
            }
            ProviderEvent::Warning { code, message } => {
                if code == "tool_call_unparseable" {
                    turn.saw_no_tool_warning = true;
                }
                if code != "multimodal_flattened" || self.iteration == 1 {
                    self.warning(&code, &message).await?;
                }
            }
            ProviderEvent::Usage(mut data) => {
                data["step"] = self.iteration.into();
                self.emit("run.usage", data).await?;
            }
            ProviderEvent::Continuation(state) => self.continuation = Some(state),
            ProviderEvent::Activity => {}
            ProviderEvent::Event { kind, data } => {
                if kind == "run.message.completed" {
                    turn.content = data["content"].as_str().map(str::to_owned);
                    turn.finish = data["finishReason"].as_str().unwrap_or("").into();
                    turn.calls =
                        serde_json::from_value(data["toolCalls"].clone()).unwrap_or_default();
                }
                self.emit(&kind, data).await?;
            }
        }
        Ok(())
    }
    async fn tools(
        &mut self,
        registry: &ToolRegistry,
        announced: &HashSet<String>,
        content: &str,
        calls: Vec<ToolCall>,
        cap: usize,
        push_assistant: bool,
    ) -> Result<(), CoreError> {
        for call in &calls {
            if !announced.contains(&call.id) {
                self.emit("run.tool.requested",json!({"toolCallId":call.id,"toolName":call.name,"argumentsJson":call.arguments_json})).await?;
            }
        }
        if calls.len() > cap {
            self.warning(
                "tool_call_cap",
                &format!(
                    "Truncated {} tool calls to {cap} per iteration.",
                    calls.len()
                ),
            )
            .await?;
        }
        if push_assistant {
            let mut assistant = ChatMessage::text("assistant", content);
            assistant.tool_calls = calls.clone();
            self.pending_messages.push(assistant.clone());
            self.messages.push(assistant);
        }
        let mut cancelled = false;
        let mut budget_error = None;
        for (index, call) in calls.iter().enumerate() {
            let outcome = if self.options.cancellation.is_cancelled() || cancelled {
                cancelled = true;
                tool_execution::failed(
                    call,
                    &CoreError::new(
                        "cancelled",
                        "Run cancelled before this tool produced a result.",
                    ),
                    self.options.limits.max_bytes,
                    json!({}),
                )
            } else if let Some(error) = &budget_error {
                tool_execution::failed(call, error, self.options.limits.max_bytes, json!({}))
            } else if index >= cap {
                tool_execution::failed(
                    call,
                    &CoreError::new(
                        "tool_call_cap",
                        &format!("Skipped: exceeded maxToolCallsPerIteration={cap}."),
                    ),
                    self.options.limits.max_bytes,
                    json!({}),
                )
            } else {
                self.emit(
                    "run.tool.running",
                    json!({"toolCallId":call.id,"toolName":call.name}),
                )
                .await?;
                let slot = Arc::new(crate::SuspendSlot::default());
                let outcome = tool_execution::execute(
                    registry,
                    call,
                    self.tool_context(call, Some(slot.clone())),
                    self.options.default_tool_timeout_ms,
                    self.options.request.budget.clone(),
                )
                .await;
                if let Some(spec) = slot.take() {
                    // The suspended call records NO tool result: the human
                    // answer is what answers it. The pending assistant and
                    // the results before it persist with the waiting event.
                    let mut data = json!({
                        "requestId": spec.request_id,
                        "toolCallId": call.id,
                        "toolName": call.name,
                        "step": self.iteration,
                        "prompt": spec.content.get("prompt").and_then(Value::as_str).unwrap_or(""),
                    });
                    if let Some(kind) = spec.content.get("kind") {
                        data["kind"] = kind.clone();
                    }
                    // Approval fields ride the event too (parity with the
                    // TypeScript `run.waiting`): the projection records the
                    // request from these bytes.
                    for key in ["argsSha256", "requestedMode", "standing"] {
                        if let Some(value) = spec.content.get(key) {
                            data[key] = value.clone();
                        }
                    }
                    if let Some(expires) = &spec.expires_at {
                        data["expiresAt"] = expires.clone().into();
                    }
                    self.emit("run.waiting", data).await?;
                    let Some(sink) = self.options.suspender.clone() else {
                        return Err(CoreError::new(
                            "suspension_unavailable",
                            "This host cannot park runs for human input; the request was refused.",
                        ));
                    };
                    sink.park(&spec, &call.id).await?;
                    self.suspended = true;
                    return Ok(());
                }
                outcome
            };
            if self.interrupted() {
                // Stop without failing the calls that never ran: the resumed
                // attempt replays the unanswered ones against receipts.
                return Ok(());
            }
            if matches!(
                outcome.data["errorCode"].as_str(),
                Some("execution_budget_exhausted" | "execution_timeout")
            ) {
                budget_error = Some(CoreError::new(
                    outcome.data["errorCode"]
                        .as_str()
                        .unwrap_or("provider_error"),
                    outcome.data["errorMessage"]
                        .as_str()
                        .unwrap_or("Execution failed"),
                ));
            }
            if let Some(budget) = &self.options.request.budget {
                if let Err(error) = budget.check() {
                    budget_error = Some(error);
                }
            }
            // The seeded history may have reconciled this call with a
            // synthesized "missing result" failure (the crash-residue rule).
            // The moment this run produces the real result, the fabricated
            // failure is superseded: the model must not see both.
            self.messages.retain(|message| {
                !(message.role == "tool"
                    && message.tool_call_id.as_deref() == Some(call.id.as_str())
                    && message.is_synthetic_missing())
            });
            let mut message = ChatMessage::text("tool", &outcome.model_json);
            message.tool_call_id = Some(call.id.clone());
            message.name = Some(call.name.clone());
            self.pending_messages.push(message.clone());
            self.messages.push(message);
            self.emit(outcome.kind, outcome.data).await?;
        }
        if let Some(error) = budget_error {
            return Err(error);
        }
        self.check()
    }
}
