use crate::{
    errors,
    jsbridge::{JsBridge, PendingKind},
    run_budget::Checkpoint,
    run_hooks::Hooks as LifecycleHooks,
    run_provider::Provider,
    run_retry::Hooks,
    run_steering::Steering,
};
use agentkit_core::{
    CancellationToken, CoreError, RunOptions, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use async_trait::async_trait;
use futures::StreamExt;
use napi::{bindgen_prelude::*, threadsafe_function::ThreadsafeFunction};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct EventGate {
    semaphore: tokio::sync::Semaphore,
}
#[async_trait]
impl agentkit_core::RunEventGate for EventGate {
    async fn wait(&self, cancellation: &CancellationToken) {
        tokio::select! {
            permit = self.semaphore.acquire() => { if let Ok(permit) = permit { permit.forget(); } },
            _ = cancellation.cancelled() => {},
        }
    }
}

struct CancelCallback {
    id: String,
    bridge: Arc<JsBridge>,
}
impl Drop for CancelCallback {
    fn drop(&mut self) {
        self.bridge
            .deliver_message(json!({"kind":"cancel", "callId":self.id}).to_string());
    }
}

struct StandaloneSuspend;
#[async_trait]
impl agentkit_core::SuspendSink for StandaloneSuspend {
    async fn park(
        &self,
        _request: &agentkit_core::HumanRequestSpec,
        _tool_call_id: &str,
    ) -> std::result::Result<(), CoreError> {
        Ok(())
    }
}

struct CallbackTool {
    definition: ToolDefinition,
    bridge: Arc<JsBridge>,
    suspendable: bool,
}
#[async_trait]
impl Tool for CallbackTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(
        &self,
        context: ToolContext,
        input: Value,
    ) -> std::result::Result<ToolResult, CoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let _cancel = CancelCallback {
            id: id.clone(),
            bridge: self.bridge.clone(),
        };
        let payload = json!({"name":self.definition.name,"context":{"runId":context.run_id,"toolCallId":context.tool_call_id,"chatId":context.chat_id,"scopeId":context.scope_id,"userId":context.user_id,"bindings":context.bindings,"limits":context.limits,"metadata":context.metadata},"input":input,"suspendable":self.suspendable});
        let callback_cancellation = CancellationToken::new();
        let value = self
            .bridge
            .clone()
            .request(PendingKind::Tool, &id, &payload, &callback_cancellation)
            .await
            .map_err(|e| CoreError::new("tool_callback_failed", &e))?;
        if let Some(request) = value.get("suspension").filter(|v| v.is_object()) {
            let generation = request["approvalGeneration"].as_str();
            let mut request_id = context.waiting_request_id();
            if let Some(generation) = generation {
                request_id.push_str(&format!(":generation:{generation}"));
            }
            let mut content = request.clone();
            let expires_at = content
                .get("expiresAt")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(object) = content.as_object_mut() {
                object.remove("expiresAt");
            }
            context.request_human(agentkit_core::HumanRequestSpec {
                request_id,
                content,
                content_hash: String::new(),
                expires_at,
            });
        }
        if let Some(error) = value.get("executionError") {
            let mut failure = CoreError::new(
                "tool_callback_failed",
                error["message"].as_str().unwrap_or("Tool failed"),
            );
            failure.retryable = error["retryable"].as_bool().unwrap_or(false);
            return Err(failure);
        }
        if value["unserializable"] == true {
            return Err(CoreError::new(
                "result_unserializable",
                "Tool result could not be serialized",
            ));
        }
        serde_json::from_value(value["result"].clone())
            .map_err(|e| CoreError::new("tool_callback_failed", &e.to_string()))
    }
}
#[napi]
pub struct NativeRunHandle {
    handle: tokio::sync::Mutex<Option<agentkit_core::RunHandle>>,
    cancellation: CancellationToken,
    force_close: CancellationToken,
    bridge: Arc<JsBridge>,
    event_gate: Arc<EventGate>,
    awaiting_ack: AtomicBool,
    budget: Option<Arc<agentkit_core::ExecutionBudget>>,
    external_provider_requests: std::sync::Mutex<Option<u64>>,
    hooks: Option<Arc<LifecycleHooks>>,
}
#[napi]
impl NativeRunHandle {
    #[napi(factory)]
    pub async fn start(options_json: String, callback: ThreadsafeFunction<String>) -> Result<Self> {
        let value: Value = serde_json::from_str(&options_json)
            .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
        let bridge = Arc::new(JsBridge::new(callback));
        let registry = registry(&value, &bridge)?;
        let cancellation = CancellationToken::new();
        if value["cancelled"] == true {
            cancellation.cancel();
        }
        let mut options = run_options(&value, cancellation.clone(), &bridge)?;
        let force_close = CancellationToken::new();
        options.tool_close = Some(force_close.clone());
        let event_gate = Arc::new(EventGate {
            semaphore: tokio::sync::Semaphore::new(0),
        });
        let hooks = configure_runtime(&value, &bridge, &cancellation, &event_gate, &mut options)?;
        let provider = provider(&value, &bridge);
        let budget = options.request.budget.clone();
        if let (Some(budget), Some(error)) = (
            &budget,
            value.get("externalBudgetError").filter(|v| v.is_object()),
        ) {
            budget.fail(CoreError::new(
                error["code"].as_str().unwrap_or("cancelled"),
                error["message"].as_str().unwrap_or("aborted"),
            ));
        }
        let handle = agentkit_core::run_chat(options, provider, Arc::new(registry));
        Ok(Self {
            handle: tokio::sync::Mutex::new(Some(handle)),
            cancellation,
            force_close,
            bridge,
            event_gate,
            awaiting_ack: AtomicBool::new(false),
            budget,
            external_provider_requests: std::sync::Mutex::new(
                value["externalBudgetError"]["providerRequests"].as_u64(),
            ),
            hooks,
        })
    }
    #[napi]
    pub async fn next(&self) -> Result<String> {
        let mut guard = self.handle.lock().await;
        if self.awaiting_ack.swap(false, Ordering::AcqRel) {
            self.event_gate.semaphore.add_permits(1);
        }
        let handle = guard
            .as_mut()
            .ok_or_else(|| errors::stable("invalid_input", "Run already completed", None))?;
        if let Some(event) = handle.events.next().await {
            self.awaiting_ack.store(true, Ordering::Release);
            return Ok(json!({"event":event}).to_string());
        }
        let mut handle = guard.take().expect("handle checked above");
        let result = (&mut handle.result)
            .await
            .map_err(|e| errors::internal(&e.to_string()))?;
        let budget_failed = matches!(
            result.terminal,
            agentkit_core::Terminal::Failed | agentkit_core::Terminal::Cancelled
        ) && self
            .budget
            .as_ref()
            .is_some_and(|budget| budget.check().is_err());
        let iterations = if budget_failed {
            self.external_provider_requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| {
                    self.budget
                        .as_ref()
                        .map(|budget| budget.snapshot().provider_requests)
                        .unwrap_or(u64::from(result.iterations))
                })
        } else {
            u64::from(result.iterations)
        };
        let appended_messages = if budget_failed {
            Vec::new()
        } else {
            result.appended_messages
        };
        let terminal = if result.suspended {
            "suspended".to_owned()
        } else {
            format!("{:?}", result.terminal).to_lowercase()
        };
        if let Some(hooks) = &self.hooks {
            hooks
                .settled(json!({"runId":result.run_id,"terminal":terminal,"iterations":iterations}))
                .await;
        }
        Ok(json!({"result":{"runId":result.run_id,"terminal":terminal,"appendedMessages":appended_messages,"iterations":iterations}}).to_string())
    }
    #[napi]
    pub fn close(&self, env: Env) -> Result<()> {
        self.force_close.cancel();
        self.cancellation.cancel();
        if self.bridge.start_closing() {
            self.bridge.detach(&env).map_err(|e| errors::internal(&e))?;
            self.bridge.mark_closed();
        }
        Ok(())
    }

    #[napi]
    pub fn fail_budget(&self, code: String, message: String, provider_requests: f64) -> Result<()> {
        if !provider_requests.is_finite()
            || provider_requests < 0.0
            || provider_requests.fract() != 0.0
            || provider_requests > 9_007_199_254_740_991.0
        {
            return Err(errors::stable(
                "invalid_input",
                "Invalid provider request count",
                None,
            ));
        }
        let mut observed = self
            .external_provider_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        observed.get_or_insert(provider_requests as u64);
        drop(observed);
        if let Some(budget) = &self.budget {
            budget.fail(CoreError::new(&code, &message));
        }
        self.cancellation.cancel();
        Ok(())
    }

    #[napi]
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

fn configure_runtime(
    value: &Value,
    bridge: &Arc<JsBridge>,
    cancellation: &CancellationToken,
    event_gate: &Arc<EventGate>,
    options: &mut RunOptions,
) -> Result<Option<Arc<LifecycleHooks>>> {
    options.event_gate = Some(event_gate.clone());
    if value["suspendable"].as_bool() == Some(true) {
        options.batch_suspension = true;
        options.suspender = Some(Arc::new(StandaloneSuspend));
    }
    if value["steering"].as_bool() == Some(true) {
        options.steering = Some(Arc::new(Steering {
            bridge: bridge.clone(),
            cancellation: cancellation.clone(),
        }));
    }
    let hooks = lifecycle_hooks(value, bridge)?;
    if let Some(hooks) = &hooks {
        options.hooks = Some(hooks.clone());
    }
    Ok(hooks)
}

fn registry(value: &Value, bridge: &Arc<JsBridge>) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for item in value["tools"].as_array().into_iter().flatten() {
        let definition: ToolDefinition = serde_json::from_value(item.clone())
            .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
        registry
            .register(Arc::new(CallbackTool {
                definition,
                bridge: bridge.clone(),
                suspendable: value["suspendable"].as_bool().unwrap_or(false),
            }))
            .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
    }
    Ok(registry)
}
fn run_options(
    value: &Value,
    cancellation: CancellationToken,
    bridge: &Arc<JsBridge>,
) -> Result<RunOptions> {
    let runtime: Arc<dyn agentkit_core::Runtime> = Arc::new(agentkit_core::SystemRuntime);
    let mut options = RunOptions {
        cancellation: cancellation.clone(),
        runtime: runtime.clone(),
        ..Default::default()
    };
    configure_request(value, &mut options)?;
    options.request.budget = make_budget(value, runtime, &cancellation, bridge)?;
    options.resume_calls = if value["resumeCalls"].is_null() {
        Vec::new()
    } else {
        serde_json::from_value(value["resumeCalls"].clone())
            .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?
    };
    options.hook_timeout_ms = value["hookTimeoutMs"].as_f64().map(|v| {
        if v.is_finite() && v > 0.0 {
            v as u64
        } else {
            0
        }
    });
    options.run_id = value["runId"].as_str().map(str::to_owned);
    options.first_seq = value["firstSeq"].as_u64().unwrap_or(0);
    options.attempt_id = value["attemptId"].as_str().map(str::to_owned);
    options.max_tool_iterations = value["maxToolIterations"].as_u64().unwrap_or(4).max(1) as u32;
    options.max_tool_calls_per_iteration =
        value["maxToolCallsPerIteration"].as_u64().unwrap_or(8) as usize;
    options.default_tool_timeout_ms = value["defaultToolTimeoutMs"].as_u64();
    options.limits = match serde_json::from_value(value["limits"].clone()) {
        Ok(limits) => limits,
        Err(_) if value["tools"].as_array().is_some_and(Vec::is_empty) => Default::default(),
        Err(error) => return Err(errors::stable("invalid_input", error.to_string(), None)),
    };
    options.chat_id = value["chatId"].as_str().map(str::to_owned);
    options.scope_id = value["scopeId"].as_str().map(str::to_owned);
    options.user_id = value["userId"].as_str().map(str::to_owned);
    options.bindings = value["bindings"].as_array().cloned().unwrap_or_default();
    options.tool_metadata = value["toolMetadata"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    Ok(options)
}
fn configure_request(value: &Value, options: &mut RunOptions) -> Result<()> {
    options.request.model = value["model"].as_str().unwrap_or("").into();
    options.request.messages = serde_json::from_value(value["messages"].clone())
        .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
    options.request.continuation = value.get("continuation").cloned().filter(|v| !v.is_null());
    if !value["continuationScope"].is_null() {
        options.request.continuation_scope = Some(
            serde_json::from_value(value["continuationScope"].clone())
                .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?,
        );
    }
    options.request.continuation_required =
        value["continuationRequired"].as_bool().unwrap_or(false);
    options.request.temperature = value["temperature"].as_f64();
    options.request.max_output_tokens = value["maxOutputTokens"].as_u64();
    Ok(())
}
fn make_budget(
    value: &Value,
    runtime: Arc<dyn agentkit_core::Runtime>,
    cancellation: &CancellationToken,
    bridge: &Arc<JsBridge>,
) -> Result<Option<Arc<agentkit_core::ExecutionBudget>>> {
    if !value["budgets"].is_null() {
        let mut limits: agentkit_core::BudgetLimits =
            serde_json::from_value(value["budgets"].clone())
                .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
        let state: Option<agentkit_core::BudgetSnapshot> = if value["budgetState"].is_null() {
            None
        } else {
            Some(
                serde_json::from_value(value["budgetState"].clone())
                    .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?,
            )
        };
        let external = value["externalBudget"].as_bool() == Some(true);
        if external {
            limits.overall_ms = None;
            limits.provider_requests = None;
            limits.tool_calls = None;
            limits.correction_passes = None;
            limits.retries = None;
        }
        let mut budget = agentkit_core::ExecutionBudget::new(limits, runtime, state.clone())
            .map_err(|e| errors::stable("invalid_input", e.message, None))?;
        if external {
            let previous = state.ok_or_else(|| {
                errors::stable("invalid_input", "Missing external budget state", None)
            })?;
            budget = budget.with_checkpoint(Arc::new(Checkpoint {
                bridge: bridge.clone(),
                cancellation: cancellation.clone(),
                previous: std::sync::Mutex::new(previous),
            }));
        }
        return Ok(Some(Arc::new(budget)));
    }
    Ok(None)
}

fn provider(value: &Value, bridge: &Arc<JsBridge>) -> Arc<dyn agentkit_core::ChatProvider> {
    let mut provider: Arc<dyn agentkit_core::ChatProvider> = Arc::new(Provider {
        bridge: bridge.clone(),
        tracks_transport_requests: value["tracksTransportRequests"].as_bool().unwrap_or(false),
    });
    if value["providerRetry"].is_object() {
        let policy = &value["providerRetry"];
        let retry = agentkit_core::RetryPolicy {
            max_attempts: policy["maxAttempts"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            base_delay_ms: policy["baseDelayMs"].as_f64().unwrap_or(f64::NAN),
            max_delay_ms: policy["maxDelayMs"].as_f64().unwrap_or(f64::NAN),
        };
        provider = Arc::new(agentkit_core::RetryProvider {
            provider,
            policy: retry,
            hooks: Arc::new(Hooks {
                bridge: bridge.clone(),
            }),
        });
    }
    provider
}

fn lifecycle_hooks(value: &Value, bridge: &Arc<JsBridge>) -> Result<Option<Arc<LifecycleHooks>>> {
    if value["hooks"].is_null() {
        return Ok(None);
    }
    let names: Vec<Vec<String>> = serde_json::from_value(value["hooks"].clone())
        .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
    if names.is_empty() {
        return Ok(None);
    }
    let timeout_ms = value["hookTimeoutMs"]
        .as_f64()
        .map(|v| {
            if v.is_finite() && v > 0.0 {
                v as u64
            } else {
                0
            }
        })
        .unwrap_or(5000);
    Ok(Some(Arc::new(LifecycleHooks {
        bridge: bridge.clone(),
        names,
        timeout_ms,
    })))
}
